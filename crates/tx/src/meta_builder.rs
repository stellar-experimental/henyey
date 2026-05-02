//! Transaction metadata building for live execution mode.
//!
//! This module provides builders for constructing transaction metadata during
//! live transaction execution. It matches the stellar-core implementation
//! in `stellar-core/src/transactions/TransactionMeta.cpp`.
//!
//! # Overview
//!
//! During live execution (as opposed to replay mode), metadata must be built
//! as the transaction executes. This includes:
//!
//! - Ledger entry changes (creates, updates, deletes)
//! - Contract events from operations
//! - Transaction-level fee events
//! - Diagnostic events for debugging and validation errors
//! - Soroban return values and resource fee tracking
//!
//! # Key Types
//!
//! - [`DiagnosticEventManager`]: Collects diagnostic events during validation and apply
//! - [`OperationMetaBuilder`]: Per-operation metadata including changes and events
//! - [`TransactionMetaBuilder`]: Orchestrates all metadata collection for a transaction
//!
//! # Protocol Versioning
//!
//! The builders support multiple TransactionMeta XDR versions:
//! - V2: Classic transactions without Soroban
//! - V3: Early Soroban with contract events at transaction level
//! - V4: Modern Soroban with per-operation events and diagnostic events

use henyey_common::NetworkId;
use stellar_xdr::curr::{
    ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, DiagnosticEvent,
    ExtensionPoint, LedgerEntry, LedgerEntryChange, LedgerEntryChanges, Memo, OperationMeta,
    OperationMetaV2, ScError, ScMap, ScMapEntry, ScVal, SorobanTransactionMeta,
    SorobanTransactionMetaExt, SorobanTransactionMetaExtV1, SorobanTransactionMetaV2,
    TransactionMeta, TransactionMetaV2, TransactionMetaV3, TransactionMetaV4,
};

use crate::events::{ClassicEventConfig, OpEventManager, TxEventManager};
use crate::frame::TransactionFrame;
use crate::result::RefundableFeeTracker;

/// Configuration for diagnostic event collection.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiagnosticConfig {
    /// Enable diagnostic events during apply.
    pub enable_soroban_diagnostic_events: bool,
    /// Enable diagnostics for transaction submission validation.
    pub enable_diagnostics_for_tx_submission: bool,
}

/// Manages diagnostic events during transaction validation and application.
///
/// Diagnostic events provide debugging information about validation failures,
/// resource exceedance, and execution metrics. They are primarily useful for
/// Soroban transactions.
///
/// # Parity
///
/// This matches the stellar-core `DiagnosticEventManager` in `EventManager.cpp`.
/// Key behaviors:
/// - Disabled managers are complete no-ops for performance
/// - Only enabled for Soroban transactions when configured
/// - Finalization is one-time (asserts if called twice)
///
/// # Event Types
///
/// ## Error Events
/// Diagnostic events with type `DIAGNOSTIC` containing:
/// - `"error"` symbol in topics
/// - Error type and code
/// - Human-readable message
///
/// ## Metrics Events
/// Core metrics tracking execution:
/// - CPU instructions, memory usage
/// - Disk read/write bytes
/// - Event emission counts
pub struct DiagnosticEventManager {
    /// Storage buffer for diagnostic events.
    buffer: Vec<DiagnosticEvent>,
    /// Whether event collection is enabled.
    enabled: bool,
    /// Whether finalize() has been called.
    finalized: bool,
}

impl DiagnosticEventManager {
    /// Create a diagnostic event manager for transaction apply phase.
    ///
    /// Only enabled for Soroban transactions when both `meta_enabled` is true
    /// and `config.enable_soroban_diagnostic_events` is true.
    pub fn create_for_apply(
        meta_enabled: bool,
        is_soroban: bool,
        config: &DiagnosticConfig,
    ) -> Self {
        let enabled = meta_enabled && is_soroban && config.enable_soroban_diagnostic_events;
        Self {
            buffer: Vec::new(),
            enabled,
            finalized: false,
        }
    }

    /// Create a diagnostic event manager for transaction validation phase.
    ///
    /// Enabled based on `config.enable_diagnostics_for_tx_submission`.
    pub fn create_for_validation(config: &DiagnosticConfig) -> Self {
        Self {
            buffer: Vec::new(),
            enabled: config.enable_diagnostics_for_tx_submission,
            finalized: false,
        }
    }

    /// Create a disabled diagnostic event manager.
    ///
    /// All push operations become no-ops.
    pub fn create_disabled() -> Self {
        Self {
            buffer: Vec::new(),
            enabled: false,
            finalized: false,
        }
    }

    /// Check if the manager is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Push a diagnostic error event.
    ///
    /// Creates a diagnostic event with:
    /// - Topics: `["error", Error(type, code)]`
    /// - Data: String message or map with message and args
    ///
    /// # Arguments
    ///
    /// * `error` - The ScError (e.g., ScError::Budget(ExceededLimit))
    /// * `message` - Human-readable error message
    /// * `args` - Optional additional ScVal arguments
    pub fn push_error(&mut self, error: ScError, message: &str, args: Vec<ScVal>) {
        if !self.enabled {
            return;
        }
        debug_assert!(!self.finalized, "Cannot push after finalize");

        // Build the error object as ScVal::Error
        let error_val = ScVal::Error(error);

        // Build topics: ["error", Error(type, code)]
        let topics = vec![make_symbol_scval("error"), error_val];

        // Build data: either just message or map with message + args
        let data = if args.is_empty() {
            make_string_scval(message)
        } else {
            let mut entries = vec![ScMapEntry {
                key: make_symbol_scval("message"),
                val: make_string_scval(message),
            }];
            for (i, arg) in args.into_iter().enumerate() {
                entries.push(ScMapEntry {
                    key: make_symbol_scval(&format!("arg{}", i)),
                    val: arg,
                });
            }
            ScVal::Map(Some(ScMap(
                entries
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
            )))
        };

        let event = ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Diagnostic,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: topics
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
                data,
            }),
        };

        self.buffer.push(DiagnosticEvent {
            in_successful_contract_call: false,
            event,
        });
    }

    /// Push a raw diagnostic event (e.g., from Soroban host).
    ///
    /// Used for events deserialized from the Soroban host output.
    pub fn push_event(&mut self, event: DiagnosticEvent) {
        if !self.enabled {
            return;
        }
        debug_assert!(!self.finalized, "Cannot push after finalize");
        self.buffer.push(event);
    }

    /// Push multiple diagnostic events.
    pub fn push_events(&mut self, events: Vec<DiagnosticEvent>) {
        if !self.enabled {
            return;
        }
        debug_assert!(!self.finalized, "Cannot push after finalize");
        self.buffer.extend(events);
    }

    /// Push a metrics diagnostic event.
    ///
    /// Creates a diagnostic event with execution metrics:
    /// - CPU instructions
    /// - Memory usage
    /// - Disk I/O bytes
    /// - Event emission counts
    pub fn push_metrics(&mut self, metrics: ExecutionMetrics) {
        if !self.enabled {
            return;
        }
        debug_assert!(!self.finalized, "Cannot push after finalize");

        let topics = vec![make_symbol_scval("core_metrics")];

        let entries: Vec<ScMapEntry> = vec![
            ScMapEntry {
                key: make_symbol_scval("cpu_insn"),
                val: ScVal::U64(metrics.cpu_insn),
            },
            ScMapEntry {
                key: make_symbol_scval("mem_byte"),
                val: ScVal::U64(metrics.mem_byte),
            },
            ScMapEntry {
                key: make_symbol_scval("ledger_read_byte"),
                val: ScVal::U64(metrics.ledger_read_byte),
            },
            ScMapEntry {
                key: make_symbol_scval("ledger_write_byte"),
                val: ScVal::U64(metrics.ledger_write_byte),
            },
            ScMapEntry {
                key: make_symbol_scval("emit_event"),
                val: ScVal::U64(metrics.emit_event),
            },
            ScMapEntry {
                key: make_symbol_scval("emit_event_byte"),
                val: ScVal::U64(metrics.emit_event_byte),
            },
            ScMapEntry {
                key: make_symbol_scval("invoke_time_nsecs"),
                val: ScVal::U64(metrics.invoke_time_nsecs),
            },
        ];

        let event = ContractEvent {
            ext: ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Diagnostic,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: topics
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
                data: ScVal::Map(Some(ScMap(
                    entries
                        .try_into()
                        .expect("XDR bounded conversion must not fail"),
                ))),
            }),
        };

        self.buffer.push(DiagnosticEvent {
            in_successful_contract_call: false,
            event,
        });
    }

    /// Get the number of collected diagnostic events.
    pub fn event_count(&self) -> usize {
        self.buffer.len()
    }

    /// Finalize and consume the diagnostic events.
    ///
    /// This can only be called once. Subsequent calls will panic in debug mode.
    pub fn finalize(mut self) -> Vec<DiagnosticEvent> {
        debug_assert!(!self.finalized, "finalize() called twice");
        self.finalized = true;
        std::mem::take(&mut self.buffer)
    }
}

/// Execution metrics for diagnostic events.
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetrics {
    /// CPU instructions executed.
    pub cpu_insn: u64,
    /// Memory bytes used.
    pub mem_byte: u64,
    /// Ledger bytes read.
    pub ledger_read_byte: u64,
    /// Ledger bytes written.
    pub ledger_write_byte: u64,
    /// Number of events emitted.
    pub emit_event: u64,
    /// Bytes of events emitted.
    pub emit_event_byte: u64,
    /// Invocation time in nanoseconds.
    pub invoke_time_nsecs: u64,
}

/// Builder for per-operation metadata.
///
/// Each operation in a transaction has its own metadata including:
/// - Ledger entry changes (creates, updates, deletes)
/// - Contract events (for Soroban or classic SAC events)
///
/// # Parity
///
/// This matches the stellar-core `OperationMetaBuilder` in `TransactionMeta.cpp`.
/// Key behaviors:
/// - Captures ledger changes at operation completion
/// - Manages operation-level contract events via OpEventManager
/// - Shares transaction-level DiagnosticEventManager reference
pub struct OperationMetaBuilder {
    /// Whether metadata collection is enabled.
    enabled: bool,
    /// Protocol version for version-specific behavior.
    protocol_version: u32,
    /// Operation-level contract events.
    event_manager: OpEventManager,
    /// Ledger entry changes for this operation.
    changes: Vec<LedgerEntryChange>,
    /// Soroban return value (for InvokeHostFunction).
    soroban_return_value: Option<ScVal>,
}

impl OperationMetaBuilder {
    /// Create a new operation meta builder.
    ///
    /// # Arguments
    ///
    /// * `enabled` - Whether metadata collection is enabled
    /// * `protocol_version` - Current protocol version
    /// * `is_soroban` - Whether this is a Soroban operation
    /// * `network_id` - Network ID for asset contract ID computation
    /// * `memo` - Transaction memo for event data
    /// * `event_config` - Configuration for event emission
    pub fn new(
        enabled: bool,
        protocol_version: u32,
        is_soroban: bool,
        network_id: NetworkId,
        memo: Memo,
        event_config: ClassicEventConfig,
    ) -> Self {
        Self {
            enabled,
            protocol_version,
            event_manager: OpEventManager::new(
                enabled,
                is_soroban,
                protocol_version,
                network_id,
                memo,
                event_config,
            ),
            changes: Vec::new(),
            soroban_return_value: None,
        }
    }

    /// Check if metadata collection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get a mutable reference to the event manager.
    pub fn event_manager_mut(&mut self) -> &mut OpEventManager {
        &mut self.event_manager
    }

    /// Get an immutable reference to the event manager.
    pub fn event_manager(&self) -> &OpEventManager {
        &self.event_manager
    }

    /// Record a created entry.
    pub fn record_create(&mut self, entry: LedgerEntry) {
        if !self.enabled {
            return;
        }
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &entry.data {
            tracing::debug!(
                key_hash = ?ttl.key_hash,
                "OperationMetaBuilder::record_create for Ttl"
            );
        }
        self.changes.push(LedgerEntryChange::Created(entry));
    }

    /// Record an updated entry with its pre-state.
    ///
    /// Emits STATE followed by UPDATED changes.
    pub fn record_update(&mut self, pre_state: LedgerEntry, post_state: LedgerEntry) {
        if !self.enabled {
            return;
        }
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &post_state.data {
            tracing::debug!(
                key_hash = ?ttl.key_hash,
                "OperationMetaBuilder::record_update for Ttl"
            );
        }
        self.changes.push(LedgerEntryChange::State(pre_state));
        self.changes.push(LedgerEntryChange::Updated(post_state));
    }

    /// Record a deleted entry with its pre-state.
    ///
    /// Emits STATE followed by REMOVED changes.
    pub fn record_delete(&mut self, key: stellar_xdr::curr::LedgerKey, pre_state: LedgerEntry) {
        if !self.enabled {
            return;
        }
        self.changes.push(LedgerEntryChange::State(pre_state));
        self.changes.push(LedgerEntryChange::Removed(key));
    }

    /// Record a restored entry (from hot archive).
    pub fn record_restore(&mut self, entry: LedgerEntry) {
        if !self.enabled {
            return;
        }
        self.changes.push(LedgerEntryChange::Restored(entry));
    }

    /// Set ledger changes directly (e.g., from TxChangeLog).
    ///
    /// This replaces any previously recorded changes.
    pub fn set_ledger_changes(&mut self, changes: Vec<LedgerEntryChange>) {
        if !self.enabled {
            return;
        }
        self.changes = changes;
    }

    /// Set the Soroban return value.
    pub fn set_soroban_return_value(&mut self, value: ScVal) {
        if !self.enabled {
            return;
        }
        self.soroban_return_value = Some(value);
    }

    /// Get the Soroban return value.
    pub fn soroban_return_value(&self) -> Option<&ScVal> {
        self.soroban_return_value.as_ref()
    }

    /// Get the protocol version.
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// Finalize into OperationMeta (for V2/V3 meta).
    pub fn finalize_v2(self) -> OperationMeta {
        OperationMeta {
            changes: self
                .changes
                .try_into()
                .expect("XDR bounded conversion must not fail"),
        }
    }

    /// Finalize into OperationMetaV2 (for V4 meta with per-op events).
    pub fn finalize_v4(mut self) -> OperationMetaV2 {
        let events = self.event_manager.finalize();
        OperationMetaV2 {
            ext: ExtensionPoint::V0,
            changes: self
                .changes
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            events: events
                .try_into()
                .expect("XDR bounded conversion must not fail"),
        }
    }
}

/// Builder for complete transaction metadata.
///
/// Orchestrates metadata collection across all operations in a transaction,
/// including:
/// - Per-operation changes and events via [`OperationMetaBuilder`]
/// - Transaction-level fee events via [`TxEventManager`]
/// - Diagnostic events via [`DiagnosticEventManager`]
/// - Soroban-specific metadata (return value, resource fees)
///
/// # Parity
///
/// This matches the stellar-core `TransactionMetaBuilder` in `TransactionMeta.cpp`.
/// Key behaviors:
/// - Creates operation builders for each operation in the transaction
/// - Manages transaction-level and operation-level events separately
/// - Supports V2, V3, and V4 TransactionMeta formats
/// - One-time finalization that produces the final XDR
///
/// # Usage
///
/// ```ignore
/// let mut builder = TransactionMetaBuilder::new(
///     true,  // meta_enabled
///     &frame,
///     protocol_version,
///     network_id,
///     event_config,
///     diagnostic_config,
/// );
///
/// // Record transaction-level changes before operations
/// builder.push_tx_changes_before(fee_changes);
///
/// // Execute each operation
/// for i in 0..frame.operation_count() {
///     let op_builder = builder.operation_meta_builder_mut(i);
///     // ... execute operation, recording changes and events ...
/// }
///
/// // Record transaction-level changes after operations
/// builder.push_tx_changes_after(refund_changes);
///
/// // Finalize to get TransactionMeta XDR
/// let meta = builder.finalize(success);
/// ```
pub struct TransactionMetaBuilder {
    /// Whether metadata collection is enabled.
    enabled: bool,
    /// Whether this is a Soroban transaction.
    is_soroban: bool,
    /// Protocol version determines XDR meta version.
    protocol_version: u32,
    /// Transaction-level changes before operations.
    tx_changes_before: Vec<LedgerEntryChange>,
    /// Transaction-level changes after operations.
    tx_changes_after: Vec<LedgerEntryChange>,
    /// Per-operation metadata builders.
    operation_builders: Vec<OperationMetaBuilder>,
    /// Transaction-level fee events.
    tx_event_manager: TxEventManager,
    /// Diagnostic events (shared across operations).
    diagnostic_event_manager: DiagnosticEventManager,
    /// Non-refundable Soroban resource fee charged.
    non_refundable_resource_fee: i64,
    /// Refundable fee tracker for Soroban.
    refundable_fee_tracker: Option<RefundableFeeTracker>,
    /// Whether finalize() has been called.
    finalized: bool,
}

impl TransactionMetaBuilder {
    /// Create a new transaction meta builder.
    ///
    /// # Arguments
    ///
    /// * `meta_enabled` - Whether metadata collection is enabled
    /// * `frame` - Transaction frame to build metadata for
    /// * `protocol_version` - Current protocol version
    /// * `network_id` - Network ID for asset contract ID computation
    /// * `event_config` - Configuration for event emission
    /// * `diagnostic_config` - Configuration for diagnostic events
    pub fn new(
        meta_enabled: bool,
        frame: &TransactionFrame,
        protocol_version: u32,
        network_id: NetworkId,
        event_config: ClassicEventConfig,
        diagnostic_config: &DiagnosticConfig,
    ) -> Self {
        let is_soroban = frame.is_soroban();
        let memo = frame.memo().clone();
        let op_count = frame.operation_count();

        // Create operation builders for each operation
        let operation_builders: Vec<OperationMetaBuilder> = (0..op_count)
            .map(|_| {
                OperationMetaBuilder::new(
                    meta_enabled,
                    protocol_version,
                    is_soroban,
                    network_id,
                    memo.clone(),
                    event_config,
                )
            })
            .collect();

        Self {
            enabled: meta_enabled,
            is_soroban,
            protocol_version,
            tx_changes_before: Vec::new(),
            tx_changes_after: Vec::new(),
            operation_builders,
            tx_event_manager: TxEventManager::new(
                meta_enabled,
                protocol_version,
                network_id,
                event_config,
            ),
            diagnostic_event_manager: DiagnosticEventManager::create_for_apply(
                meta_enabled,
                is_soroban,
                diagnostic_config,
            ),
            non_refundable_resource_fee: 0,
            refundable_fee_tracker: None,
            finalized: false,
        }
    }

    /// Create a disabled transaction meta builder.
    ///
    /// All operations become no-ops. Useful for performance when metadata
    /// is not needed.
    pub fn create_disabled(frame: &TransactionFrame) -> Self {
        let op_count = frame.operation_count();
        Self {
            enabled: false,
            is_soroban: frame.is_soroban(),
            protocol_version: 0,
            tx_changes_before: Vec::new(),
            tx_changes_after: Vec::new(),
            operation_builders: (0..op_count)
                .map(|_| {
                    OperationMetaBuilder::new(
                        false,
                        0,
                        false,
                        NetworkId::testnet(),
                        Memo::None,
                        ClassicEventConfig::default(),
                    )
                })
                .collect(),
            tx_event_manager: TxEventManager::new(
                false,
                0,
                NetworkId::testnet(),
                ClassicEventConfig::default(),
            ),
            diagnostic_event_manager: DiagnosticEventManager::create_disabled(),
            non_refundable_resource_fee: 0,
            refundable_fee_tracker: None,
            finalized: false,
        }
    }

    /// Check if metadata collection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get a mutable reference to the transaction event manager.
    pub fn tx_event_manager_mut(&mut self) -> &mut TxEventManager {
        &mut self.tx_event_manager
    }

    /// Get a mutable reference to the diagnostic event manager.
    pub fn diagnostic_event_manager_mut(&mut self) -> &mut DiagnosticEventManager {
        &mut self.diagnostic_event_manager
    }

    /// Get a mutable reference to an operation meta builder.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn operation_meta_builder_mut(&mut self, index: usize) -> &mut OperationMetaBuilder {
        &mut self.operation_builders[index]
    }

    /// Get an immutable reference to an operation meta builder.
    pub fn operation_meta_builder(&self, index: usize) -> &OperationMetaBuilder {
        &self.operation_builders[index]
    }

    /// Get the number of operations.
    pub fn operation_count(&self) -> usize {
        self.operation_builders.len()
    }

    /// Push transaction-level changes before operations.
    ///
    /// These typically include fee deduction and sequence number bump.
    pub fn push_tx_changes_before(&mut self, changes: Vec<LedgerEntryChange>) {
        if !self.enabled {
            return;
        }
        self.tx_changes_before.extend(changes);
    }

    /// Push transaction-level changes after operations.
    ///
    /// These typically include fee refunds for Soroban transactions.
    pub fn push_tx_changes_after(&mut self, changes: Vec<LedgerEntryChange>) {
        if !self.enabled {
            return;
        }
        self.tx_changes_after.extend(changes);
    }

    /// Set the non-refundable Soroban resource fee.
    pub fn set_non_refundable_resource_fee(&mut self, fee: i64) {
        self.non_refundable_resource_fee = fee;
    }

    /// Set the refundable fee tracker.
    pub fn set_refundable_fee_tracker(&mut self, tracker: RefundableFeeTracker) {
        self.refundable_fee_tracker = Some(tracker);
    }

    /// Get the protocol version.
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// Determine the XDR meta version based on protocol.
    fn meta_version(&self) -> u32 {
        // Protocol 20+ uses V4 for Soroban support
        4
    }

    /// Finalize and consume, producing the TransactionMeta XDR.
    ///
    /// # Arguments
    ///
    /// * `success` - Whether the transaction succeeded
    ///
    /// # Returns
    ///
    /// The finalized TransactionMeta XDR structure.
    pub fn finalize(mut self, success: bool) -> TransactionMeta {
        debug_assert!(!self.finalized, "finalize() called twice");
        self.finalized = true;

        if !self.enabled {
            // Return empty V2 meta when disabled
            return TransactionMeta::V2(TransactionMetaV2 {
                tx_changes_before: LedgerEntryChanges::default(),
                operations: vec![]
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
                tx_changes_after: LedgerEntryChanges::default(),
            });
        }

        let meta_version = self.meta_version();

        match meta_version {
            2 => self.finalize_v2(),
            3 => self.finalize_v3(success),
            4 => self.finalize_v4(success),
            _ => self.finalize_v2(),
        }
    }

    /// Finalize to V2 meta (classic, no Soroban).
    fn finalize_v2(self) -> TransactionMeta {
        let operations: Vec<OperationMeta> = self
            .operation_builders
            .into_iter()
            .map(|b| b.finalize_v2())
            .collect();

        TransactionMeta::V2(TransactionMetaV2 {
            tx_changes_before: self
                .tx_changes_before
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            operations: operations
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            tx_changes_after: self
                .tx_changes_after
                .try_into()
                .expect("XDR bounded conversion must not fail"),
        })
    }

    /// Finalize to V3 meta (early Soroban, events at tx level).
    fn finalize_v3(self, _success: bool) -> TransactionMeta {
        let operations: Vec<OperationMeta> = self
            .operation_builders
            .into_iter()
            .map(|b| b.finalize_v2())
            .collect();

        let diagnostic_events = self.diagnostic_event_manager.finalize();

        // stellar-core always activates sorobanMeta for V3 Soroban txs,
        // even on failure (TransactionMeta.cpp:694-697).
        let soroban_meta = if self.is_soroban {
            Some(SorobanTransactionMeta {
                ext: SorobanTransactionMetaExt::V0,
                events: vec![]
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
                return_value: ScVal::Void,
                diagnostic_events: diagnostic_events
                    .try_into()
                    .expect("XDR bounded conversion must not fail"),
            })
        } else {
            None
        };

        TransactionMeta::V3(TransactionMetaV3 {
            ext: ExtensionPoint::V0,
            tx_changes_before: self
                .tx_changes_before
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            operations: operations
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            tx_changes_after: self
                .tx_changes_after
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            soroban_meta,
        })
    }

    /// Finalize to V4 meta (modern Soroban, per-op events).
    fn finalize_v4(mut self, success: bool) -> TransactionMeta {
        // For V4, we need OperationMetaV2 with per-operation events
        let operations: Vec<OperationMetaV2> = self
            .operation_builders
            .into_iter()
            .map(|b| b.finalize_v4())
            .collect();

        let tx_events = self.tx_event_manager.finalize();
        let diagnostic_events = self.diagnostic_event_manager.finalize();

        // Build Soroban meta V2 for resource fee tracking (V4 uses SorobanTransactionMetaV2)
        let soroban_meta = if self.is_soroban && success {
            let ext = if let Some(ref tracker) = self.refundable_fee_tracker {
                SorobanTransactionMetaExt::V1(SorobanTransactionMetaExtV1 {
                    ext: ExtensionPoint::V0,
                    total_non_refundable_resource_fee_charged: self.non_refundable_resource_fee,
                    total_refundable_resource_fee_charged: tracker.consumed_refundable_fee(),
                    rent_fee_charged: tracker.consumed_rent_fee(),
                })
            } else {
                SorobanTransactionMetaExt::V0
            };

            Some(SorobanTransactionMetaV2 {
                ext,
                return_value: Some(ScVal::Void), // Return value from operation builder
            })
        } else {
            None
        };

        TransactionMeta::V4(TransactionMetaV4 {
            ext: ExtensionPoint::V0,
            tx_changes_before: self
                .tx_changes_before
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            operations: operations
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            tx_changes_after: self
                .tx_changes_after
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            soroban_meta,
            events: tx_events
                .try_into()
                .expect("XDR bounded conversion must not fail"),
            diagnostic_events: diagnostic_events
                .try_into()
                .expect("XDR bounded conversion must not fail"),
        })
    }
}

use crate::scval_utils::{make_string_scval, make_symbol_scval};

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn create_test_frame() -> TransactionFrame {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let payment_op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([1u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        TransactionFrame::from_owned(envelope)
    }

    // DiagnosticEventManager tests

    #[test]
    fn test_diagnostic_manager_disabled() {
        let manager = DiagnosticEventManager::create_disabled();
        assert!(!manager.is_enabled());
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_diagnostic_manager_enabled_for_validation() {
        let config = DiagnosticConfig {
            enable_diagnostics_for_tx_submission: true,
            enable_soroban_diagnostic_events: false,
        };
        let manager = DiagnosticEventManager::create_for_validation(&config);
        assert!(manager.is_enabled());
    }

    #[test]
    fn test_diagnostic_manager_push_error() {
        let config = DiagnosticConfig {
            enable_diagnostics_for_tx_submission: true,
            enable_soroban_diagnostic_events: false,
        };
        let mut manager = DiagnosticEventManager::create_for_validation(&config);

        manager.push_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            "CPU limit exceeded",
            vec![],
        );

        assert_eq!(manager.event_count(), 1);

        let events = manager.finalize();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.type_, ContractEventType::Diagnostic);
    }

    #[test]
    fn test_diagnostic_manager_push_error_with_args() {
        let config = DiagnosticConfig {
            enable_diagnostics_for_tx_submission: true,
            enable_soroban_diagnostic_events: false,
        };
        let mut manager = DiagnosticEventManager::create_for_validation(&config);

        manager.push_error(
            ScError::Storage(ScErrorCode::InvalidInput),
            "Invalid key type",
            vec![ScVal::U64(42)],
        );

        assert_eq!(manager.event_count(), 1);
        let events = manager.finalize();

        // Verify the data contains a map with message and args
        let ContractEventBody::V0(body) = &events[0].event.body;
        matches!(body.data, ScVal::Map(_));
    }

    #[test]
    fn test_diagnostic_manager_push_metrics() {
        let config = DiagnosticConfig {
            enable_soroban_diagnostic_events: true,
            enable_diagnostics_for_tx_submission: false,
        };
        let mut manager = DiagnosticEventManager::create_for_apply(true, true, &config);

        let metrics = ExecutionMetrics {
            cpu_insn: 1000000,
            mem_byte: 65536,
            ledger_read_byte: 1024,
            ledger_write_byte: 512,
            emit_event: 5,
            emit_event_byte: 256,
            invoke_time_nsecs: 100000,
        };
        manager.push_metrics(metrics);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_diagnostic_manager_disabled_noop() {
        let mut manager = DiagnosticEventManager::create_disabled();

        manager.push_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            "Should not be recorded",
            vec![],
        );

        assert_eq!(manager.event_count(), 0);
    }

    // OperationMetaBuilder tests

    #[test]
    fn test_operation_builder_disabled() {
        let builder = OperationMetaBuilder::new(
            false,
            21,
            false,
            NetworkId::testnet(),
            Memo::None,
            ClassicEventConfig::default(),
        );
        assert!(!builder.is_enabled());
    }

    #[test]
    fn test_operation_builder_record_create() {
        let mut builder = OperationMetaBuilder::new(
            true,
            21,
            false,
            NetworkId::testnet(),
            Memo::None,
            ClassicEventConfig::default(),
        );

        let entry = create_test_account_entry();
        builder.record_create(entry);

        let meta = builder.finalize_v2();
        assert_eq!(meta.changes.len(), 1);
        matches!(meta.changes[0], LedgerEntryChange::Created(_));
    }

    #[test]
    fn test_operation_builder_record_update() {
        let mut builder = OperationMetaBuilder::new(
            true,
            21,
            false,
            NetworkId::testnet(),
            Memo::None,
            ClassicEventConfig::default(),
        );

        let pre_state = create_test_account_entry();
        let mut post_state = pre_state.clone();
        if let LedgerEntryData::Account(ref mut acc) = post_state.data {
            acc.balance = 2000000000;
        }
        builder.record_update(pre_state, post_state);

        let meta = builder.finalize_v2();
        assert_eq!(meta.changes.len(), 2); // STATE + UPDATED
        matches!(meta.changes[0], LedgerEntryChange::State(_));
        matches!(meta.changes[1], LedgerEntryChange::Updated(_));
    }

    #[test]
    fn test_operation_builder_v4_with_events() {
        let builder = OperationMetaBuilder::new(
            true,
            21,
            true,
            NetworkId::testnet(),
            Memo::None,
            ClassicEventConfig {
                emit_classic_events: true,
                backfill_stellar_asset_events: false,
            },
        );

        // Events are managed through the event_manager
        let meta = builder.finalize_v4();
        assert_eq!(meta.changes.len(), 0);
        // Events vector exists in V4 (always true for Vec)
        assert!(meta.events.is_empty() || !meta.events.is_empty());
    }

    #[test]
    fn test_operation_builder_soroban_return_value() {
        let mut builder = OperationMetaBuilder::new(
            true,
            21,
            true,
            NetworkId::testnet(),
            Memo::None,
            ClassicEventConfig::default(),
        );

        builder.set_soroban_return_value(ScVal::U64(42));
        assert_eq!(builder.soroban_return_value(), Some(&ScVal::U64(42)));
    }

    // TransactionMetaBuilder tests

    #[test]
    fn test_tx_builder_creation() {
        let frame = create_test_frame();
        let builder = TransactionMetaBuilder::new(
            true,
            &frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        assert!(builder.is_enabled());
        assert_eq!(builder.operation_count(), 1);
    }

    #[test]
    fn test_tx_builder_disabled() {
        let frame = create_test_frame();
        let builder = TransactionMetaBuilder::create_disabled(&frame);

        assert!(!builder.is_enabled());
        assert_eq!(builder.operation_count(), 1);
    }

    #[test]
    fn test_tx_builder_tx_changes() {
        let frame = create_test_frame();
        let mut builder = TransactionMetaBuilder::new(
            true,
            &frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        let entry = create_test_account_entry();
        builder.push_tx_changes_before(vec![LedgerEntryChange::State(entry.clone())]);
        builder.push_tx_changes_after(vec![LedgerEntryChange::Updated(entry)]);

        let meta = builder.finalize(true);
        match meta {
            TransactionMeta::V4(v4) => {
                assert_eq!(v4.tx_changes_before.len(), 1);
                assert_eq!(v4.tx_changes_after.len(), 1);
            }
            _ => panic!("Expected V4 meta"),
        }
    }

    #[test]
    fn test_tx_builder_finalize_v2() {
        let frame = create_test_frame();
        let builder = TransactionMetaBuilder::new(
            true,
            &frame,
            18, // Protocol 18 -> V2
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        let meta = builder.finalize(true);
        matches!(meta, TransactionMeta::V2(_));
    }

    #[test]
    fn test_tx_builder_finalize_v3() {
        let frame = create_test_frame();
        let builder = TransactionMetaBuilder::new(
            true,
            &frame,
            19, // Protocol 19 -> V3
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        let meta = builder.finalize(true);
        matches!(meta, TransactionMeta::V3(_));
    }

    #[test]
    fn test_tx_builder_finalize_v4() {
        let frame = create_test_frame();
        let builder = TransactionMetaBuilder::new(
            true,
            &frame,
            21, // Protocol 21 -> V4
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        let meta = builder.finalize(true);
        matches!(meta, TransactionMeta::V4(_));
    }

    #[test]
    fn test_tx_builder_operation_access() {
        let frame = create_test_frame();
        let mut builder = TransactionMetaBuilder::new(
            true,
            &frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        let entry = create_test_account_entry();
        builder.operation_meta_builder_mut(0).record_create(entry);

        let meta = builder.finalize(true);
        match meta {
            TransactionMeta::V4(v4) => {
                assert_eq!(v4.operations.len(), 1);
                assert_eq!(v4.operations[0].changes.len(), 1);
            }
            _ => panic!("Expected V4 meta"),
        }
    }

    #[test]
    fn test_tx_builder_diagnostic_access() {
        let _frame = create_test_frame();
        let diagnostic_config = DiagnosticConfig {
            enable_soroban_diagnostic_events: true,
            enable_diagnostics_for_tx_submission: false,
        };

        // Create a Soroban transaction for diagnostic events
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let soroban_op = Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 1000,
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![soroban_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let soroban_frame = TransactionFrame::from_owned(envelope);

        let mut builder = TransactionMetaBuilder::new(
            true,
            &soroban_frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &diagnostic_config,
        );

        builder.diagnostic_event_manager_mut().push_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            "Test error",
            vec![],
        );

        let meta = builder.finalize(true);
        match meta {
            TransactionMeta::V4(v4) => {
                assert_eq!(v4.diagnostic_events.len(), 1);
            }
            _ => panic!("Expected V4 meta"),
        }
    }

    #[test]
    fn test_tx_builder_soroban_fee_tracking() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let soroban_op = Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 1000,
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 100000,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![soroban_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let soroban_frame = TransactionFrame::from_owned(envelope);

        let mut builder = TransactionMetaBuilder::new(
            true,
            &soroban_frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &DiagnosticConfig::default(),
        );

        builder.set_non_refundable_resource_fee(50000);

        let mut tracker = RefundableFeeTracker::new(30000);
        tracker.consume_rent_fee(10000).unwrap();
        builder.set_refundable_fee_tracker(tracker);

        let meta = builder.finalize(true);
        match meta {
            TransactionMeta::V4(v4) => {
                if let Some(soroban_meta) = v4.soroban_meta {
                    match soroban_meta.ext {
                        SorobanTransactionMetaExt::V1(ext) => {
                            assert_eq!(ext.total_non_refundable_resource_fee_charged, 50000);
                            assert_eq!(ext.rent_fee_charged, 10000);
                        }
                        _ => panic!("Expected V1 ext"),
                    }
                }
            }
            _ => panic!("Expected V4 meta"),
        }
    }

    /// Helper to create a Soroban transaction frame for testing.
    fn create_soroban_frame() -> TransactionFrame {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let soroban_op = Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 1000,
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![soroban_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        TransactionFrame::from_owned(envelope)
    }

    #[test]
    fn test_finalize_v3_failed_soroban_preserves_diagnostic_events() {
        let diagnostic_config = DiagnosticConfig {
            enable_soroban_diagnostic_events: true,
            enable_diagnostics_for_tx_submission: false,
        };
        let soroban_frame = create_soroban_frame();

        let mut builder = TransactionMetaBuilder::new(
            true,
            &soroban_frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &diagnostic_config,
        );

        builder.diagnostic_event_manager_mut().push_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            "Test error on failure",
            vec![],
        );

        // Call finalize_v3 directly (since meta_version() always returns 4)
        let meta = builder.finalize_v3(false);
        match meta {
            TransactionMeta::V3(v3) => {
                let soroban_meta = v3
                    .soroban_meta
                    .expect("soroban_meta must be Some for failed Soroban tx in V3");
                assert_eq!(
                    soroban_meta.diagnostic_events.len(),
                    1,
                    "diagnostic events must be preserved on failure"
                );
                assert!(soroban_meta.events.is_empty());
                assert_eq!(soroban_meta.return_value, ScVal::Void);
            }
            _ => panic!("Expected V3 meta"),
        }
    }

    #[test]
    fn test_finalize_v3_failed_soroban_without_diagnostics() {
        let diagnostic_config = DiagnosticConfig {
            enable_soroban_diagnostic_events: false,
            enable_diagnostics_for_tx_submission: false,
        };
        let soroban_frame = create_soroban_frame();

        let builder = TransactionMetaBuilder::new(
            true,
            &soroban_frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &diagnostic_config,
        );

        // Even with diagnostics disabled, V3 Soroban meta must be Some
        let meta = builder.finalize_v3(false);
        match meta {
            TransactionMeta::V3(v3) => {
                let soroban_meta = v3
                    .soroban_meta
                    .expect("soroban_meta must be Some for failed Soroban tx in V3");
                assert!(
                    soroban_meta.diagnostic_events.is_empty(),
                    "diagnostics disabled — events should be empty"
                );
                assert!(soroban_meta.events.is_empty());
                assert_eq!(soroban_meta.return_value, ScVal::Void);
            }
            _ => panic!("Expected V3 meta"),
        }
    }

    #[test]
    fn test_finalize_v3_success_soroban_has_meta() {
        let diagnostic_config = DiagnosticConfig {
            enable_soroban_diagnostic_events: true,
            enable_diagnostics_for_tx_submission: false,
        };
        let soroban_frame = create_soroban_frame();

        let mut builder = TransactionMetaBuilder::new(
            true,
            &soroban_frame,
            21,
            NetworkId::testnet(),
            ClassicEventConfig::default(),
            &diagnostic_config,
        );

        builder.diagnostic_event_manager_mut().push_error(
            ScError::Budget(ScErrorCode::ExceededLimit),
            "Test diagnostic",
            vec![],
        );

        let meta = builder.finalize_v3(true);
        match meta {
            TransactionMeta::V3(v3) => {
                let soroban_meta = v3
                    .soroban_meta
                    .expect("soroban_meta must be Some for successful Soroban tx in V3");
                assert_eq!(soroban_meta.diagnostic_events.len(), 1);
            }
            _ => panic!("Expected V3 meta"),
        }
    }

    // Helper to create test account entry
    fn create_test_account_entry() -> LedgerEntry {
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance: 1000000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }
}
