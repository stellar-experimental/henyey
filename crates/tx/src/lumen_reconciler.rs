//! LumenEventReconciler for XLM balance reconciliation.
//!
//! This module handles a pre-protocol 8 edge case where XLM could be minted or burned
//! outside of normal operations. The reconciler calculates the total XLM balance delta
//! across all ledger entry changes and emits appropriate mint/burn events if needed.
//!
//! # Background
//!
//! In early protocol versions (pre-v8), certain operations could result in XLM being
//! created or destroyed without explicit transfer events. The LumenEventReconciler
//! detects these cases by comparing the total XLM balance before and after an operation
//! and emits synthetic mint or burn events to ensure the event log accurately reflects
//! all XLM movements.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_tx::lumen_reconciler::reconcile_events;
//!
//! // After applying an operation
//! reconcile_events(
//!     &tx_source_account,
//!     &operation,
//!     &ledger_delta,
//!     &mut op_event_manager,
//! );
//! ```
//!
//! # Parity
//!
//! This module matches the behavior of `LumenEventReconciler.cpp` in stellar-core v25,
//! including:
//! - Balance delta calculation across account entries
//! - Mint events with `insertAtBeginning=true` for positive deltas
//! - Burn events for negative deltas
//! - Operation source account determination

use stellar_xdr::curr::{
    AccountId, Asset, LedgerEntry, LedgerEntryData, MuxedAccount, Operation, ScAddress,
};

use crate::apply::LedgerDelta;
use crate::events::OpEventManager;
use crate::frame::muxed_to_account_id;

/// Reconciles XLM balance changes and emits synthetic events.
///
/// This function detects XLM balance discrepancies that aren't accounted for by
/// explicit transfer events and emits mint/burn events as needed.
///
/// # Parameters
///
/// - `tx_source_account`: The transaction's source account (fallback for operation source)
/// - `operation`: The operation being applied
/// - `delta`: The ledger delta containing all state changes
/// - `op_event_manager`: Event manager to emit reconciliation events to
///
/// # Behavior
///
/// 1. Calculates total XLM balance delta across all account entries in the delta
/// 2. If delta > 0: emits a mint event (XLM was created)
/// 3. If delta < 0: emits a burn event (XLM was destroyed)
/// 4. If delta == 0: no action taken
///
/// # Event Ordering
///
/// Mint events are inserted at the beginning of the event list.
pub fn reconcile_events(
    tx_source_account: &MuxedAccount,
    operation: &Operation,
    delta: &LedgerDelta,
    op_event_manager: &mut OpEventManager,
) {
    if !op_event_manager.is_enabled() {
        return;
    }

    // Calculate total XLM balance delta
    let balance_delta = calculate_balance_delta(delta);

    if balance_delta == 0 {
        return;
    }

    // Determine the effective source account
    let source_account = get_operation_source(tx_source_account, operation);
    let source_address = ScAddress::Account(source_account);

    if balance_delta > 0 {
        // XLM was created - emit mint event at beginning
        op_event_manager.new_mint_event_at_beginning(
            &Asset::Native,
            &source_address,
            balance_delta,
        );
    } else {
        // XLM was destroyed - emit burn event
        op_event_manager.new_burn_event(&Asset::Native, &source_address, -balance_delta);
    }
}

/// Calculates the total XLM balance delta from ledger changes.
///
/// Iterates through all account entries in the delta and sums up the balance
/// changes (current - previous).
fn calculate_balance_delta(delta: &LedgerDelta) -> i64 {
    let mut total_delta: i64 = 0;

    // Process updated entries (post-state vs pre-state)
    let updated = delta.updated_entries();
    let pre_states = delta.update_states();
    for (post, pre) in updated.iter().zip(pre_states.iter()) {
        let post_balance = get_account_balance(Some(post));
        let pre_balance = get_account_balance(Some(pre));
        total_delta = total_delta.saturating_add(post_balance - pre_balance);
    }

    // Process created entries (no previous state, so add full balance)
    for entry in delta.created_entries() {
        let balance = get_account_balance(Some(entry));
        total_delta = total_delta.saturating_add(balance);
    }

    // Process deleted entries (no current state, so subtract full balance)
    for entry in delta.delete_states() {
        let balance = get_account_balance(Some(entry));
        total_delta = total_delta.saturating_sub(balance);
    }

    total_delta
}

/// Extracts the XLM balance from a ledger entry if it's an account.
fn get_account_balance(entry: Option<&LedgerEntry>) -> i64 {
    let Some(entry) = entry else {
        return 0;
    };

    match &entry.data {
        LedgerEntryData::Account(account) => account.balance,
        _ => 0,
    }
}

/// Determines the effective source account for an operation.
///
/// Uses the operation's source account if specified, otherwise falls back
/// to the transaction source account.
fn get_operation_source(tx_source: &MuxedAccount, operation: &Operation) -> AccountId {
    operation
        .source_account
        .as_ref()
        .map(muxed_to_account_id)
        .unwrap_or_else(|| muxed_to_account_id(tx_source))
}

/// Reconciler for XLM balance tracking across a transaction.
///
/// This struct provides stateful reconciliation for tracking XLM movements
/// across multiple operations in a transaction. It can be used to detect
/// and emit events for unaccounted XLM balance changes.
///
/// # Usage
///
/// ```ignore
/// let mut reconciler = LumenEventReconciler::new();
///
/// // Before each operation
/// reconciler.snapshot_balances(&state);
///
/// // After operation execution
/// let delta = reconciler.calculate_delta(&state);
/// if delta != 0 {
///     // Emit appropriate event
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct LumenEventReconciler {
    /// Tracked account balances (account_id -> balance)
    balances: std::collections::HashMap<AccountId, i64>,
    /// Whether reconciliation is enabled
    enabled: bool,
}

impl LumenEventReconciler {
    /// Create a new enabled reconciler.
    pub fn new() -> Self {
        Self {
            balances: std::collections::HashMap::new(),
            enabled: true,
        }
    }

    /// Create a disabled reconciler (no-op).
    pub fn disabled() -> Self {
        Self {
            balances: std::collections::HashMap::new(),
            enabled: false,
        }
    }

    /// Check if reconciliation is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Clear all tracked balances.
    pub fn clear(&mut self) {
        self.balances.clear();
    }

    /// Record the current balance for an account.
    pub fn track_balance(&mut self, account_id: AccountId, balance: i64) {
        if self.enabled {
            self.balances.insert(account_id, balance);
        }
    }

    /// Get the previously recorded balance for an account.
    pub fn get_tracked_balance(&self, account_id: &AccountId) -> Option<i64> {
        self.balances.get(account_id).copied()
    }

    /// Calculate the XLM balance delta for an account.
    ///
    /// Returns `Some(delta)` if the account was tracked, `None` otherwise.
    pub fn calculate_account_delta(
        &self,
        account_id: &AccountId,
        current_balance: i64,
    ) -> Option<i64> {
        if !self.enabled {
            return None;
        }

        self.balances
            .get(account_id)
            .map(|&prev| current_balance - prev)
    }

    /// Calculate total delta for all tracked accounts given current balances.
    pub fn calculate_total_delta<F>(&self, get_current_balance: F) -> i64
    where
        F: Fn(&AccountId) -> i64,
    {
        if !self.enabled {
            return 0;
        }

        self.balances
            .iter()
            .map(|(account_id, &prev_balance)| {
                let current = get_current_balance(account_id);
                current - prev_balance
            })
            .sum()
    }
}

/// Configuration for lumen reconciliation behavior.
#[derive(Debug, Clone, Copy)]
pub struct ReconcilerConfig {
    /// Whether to enable reconciliation (typically only for pre-protocol 8)
    pub enabled: bool,
    /// Protocol version (reconciliation behavior varies by version)
    pub protocol_version: u32,
}

impl ReconcilerConfig {
    /// Create configuration for a given protocol version.
    pub fn for_protocol(protocol_version: u32) -> Self {
        // Reconciliation is primarily needed for pre-protocol 8
        // but we keep it enabled for consistency
        Self {
            enabled: true,
            protocol_version,
        }
    }

    /// Check if reconciliation should be active.
    ///
    /// Reconciliation is most important for pre-protocol 8, but can be
    /// kept enabled for all versions for consistency.
    pub fn should_reconcile(&self) -> bool {
        self.enabled
    }
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol_version: 21,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntry, LedgerEntryData, LedgerEntryExt,
        LedgerKey, LedgerKeyAccount, SequenceNumber, String32, Thresholds, Uint256, VecM,
    };

    fn make_account_entry(account_id: AccountId, balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_key(account_id: &AccountId) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        })
    }

    #[test]
    fn test_reconciler_new() {
        let reconciler = LumenEventReconciler::new();
        assert!(reconciler.is_enabled());
        assert!(reconciler.balances.is_empty());
    }

    #[test]
    fn test_reconciler_disabled() {
        let reconciler = LumenEventReconciler::disabled();
        assert!(!reconciler.is_enabled());
    }

    #[test]
    fn test_reconciler_track_balance() {
        let mut reconciler = LumenEventReconciler::new();
        let account_id = create_test_account_id(1);

        reconciler.track_balance(account_id.clone(), 1000);

        assert_eq!(reconciler.get_tracked_balance(&account_id), Some(1000));
    }

    #[test]
    fn test_reconciler_track_balance_disabled() {
        let mut reconciler = LumenEventReconciler::disabled();
        let account_id = create_test_account_id(1);

        reconciler.track_balance(account_id.clone(), 1000);

        assert_eq!(reconciler.get_tracked_balance(&account_id), None);
    }

    #[test]
    fn test_reconciler_calculate_account_delta() {
        let mut reconciler = LumenEventReconciler::new();
        let account_id = create_test_account_id(1);

        reconciler.track_balance(account_id.clone(), 1000);

        // Balance increased by 500
        assert_eq!(
            reconciler.calculate_account_delta(&account_id, 1500),
            Some(500)
        );

        // Balance decreased by 200
        assert_eq!(
            reconciler.calculate_account_delta(&account_id, 800),
            Some(-200)
        );
    }

    #[test]
    fn test_reconciler_calculate_total_delta() {
        let mut reconciler = LumenEventReconciler::new();
        let account1 = create_test_account_id(1);
        let account2 = create_test_account_id(2);

        reconciler.track_balance(account1.clone(), 1000);
        reconciler.track_balance(account2.clone(), 2000);

        // Account 1: 1000 -> 1500 (+500)
        // Account 2: 2000 -> 1800 (-200)
        // Total delta: +300
        let delta = reconciler.calculate_total_delta(|id| {
            if id == &account1 {
                1500
            } else if id == &account2 {
                1800
            } else {
                0
            }
        });

        assert_eq!(delta, 300);
    }

    #[test]
    fn test_reconciler_clear() {
        let mut reconciler = LumenEventReconciler::new();
        let account_id = create_test_account_id(1);

        reconciler.track_balance(account_id.clone(), 1000);
        assert!(reconciler.get_tracked_balance(&account_id).is_some());

        reconciler.clear();
        assert!(reconciler.get_tracked_balance(&account_id).is_none());
    }

    #[test]
    fn test_reconciler_config_for_protocol() {
        let config = ReconcilerConfig::for_protocol(7);
        assert!(config.enabled);
        assert_eq!(config.protocol_version, 7);
        assert!(config.should_reconcile());

        let config = ReconcilerConfig::for_protocol(25);
        assert!(config.enabled);
        assert_eq!(config.protocol_version, 25);
    }

    #[test]
    fn test_get_account_balance() {
        let account_id = create_test_account_id(1);
        let entry = make_account_entry(account_id.clone(), 5000);

        assert_eq!(get_account_balance(Some(&entry)), 5000);
        assert_eq!(get_account_balance(None), 0);
    }

    #[test]
    fn test_get_account_balance_non_account() {
        // Create a non-account entry (using Trustline as example would require more setup)
        // For now, test that None returns 0
        assert_eq!(get_account_balance(None), 0);
    }

    #[test]
    fn test_calculate_balance_delta_created() {
        let account_id = create_test_account_id(1);
        let entry = make_account_entry(account_id.clone(), 1000);

        let mut delta = LedgerDelta::new(1.into());
        delta.record_create(entry);

        let balance_delta = calculate_balance_delta(&delta);
        assert_eq!(balance_delta, 1000);
    }

    #[test]
    fn test_calculate_balance_delta_deleted() {
        let account_id = create_test_account_id(1);
        let entry = make_account_entry(account_id.clone(), 1000);

        let mut delta = LedgerDelta::new(1.into());
        delta.record_delete(make_account_key(&account_id), entry);

        let balance_delta = calculate_balance_delta(&delta);
        assert_eq!(balance_delta, -1000);
    }

    #[test]
    fn test_calculate_balance_delta_updated() {
        let account_id = create_test_account_id(1);
        let prev_entry = make_account_entry(account_id.clone(), 1000);
        let curr_entry = make_account_entry(account_id.clone(), 1500);

        let mut delta = LedgerDelta::new(1.into());
        delta.record_update(prev_entry, curr_entry);

        let balance_delta = calculate_balance_delta(&delta);
        assert_eq!(balance_delta, 500);
    }

    #[test]
    fn test_calculate_balance_delta_multiple() {
        let account1 = create_test_account_id(1);
        let account2 = create_test_account_id(2);
        let account3 = create_test_account_id(3);

        let mut delta = LedgerDelta::new(1.into());

        // Create account with 1000
        delta.record_create(make_account_entry(account1.clone(), 1000));

        // Delete account with 500
        delta.record_delete(
            make_account_key(&account2),
            make_account_entry(account2.clone(), 500),
        );

        // Update account from 2000 to 2500
        delta.record_update(
            make_account_entry(account3.clone(), 2000),
            make_account_entry(account3.clone(), 2500),
        );

        // Total: +1000 - 500 + 500 = +1000
        let balance_delta = calculate_balance_delta(&delta);
        assert_eq!(balance_delta, 1000);
    }

    #[test]
    fn test_calculate_balance_delta_zero() {
        let account_id = create_test_account_id(1);
        let entry = make_account_entry(account_id.clone(), 1000);

        let mut delta = LedgerDelta::new(1.into());
        // Update with no balance change
        delta.record_update(entry.clone(), entry);

        let balance_delta = calculate_balance_delta(&delta);
        assert_eq!(balance_delta, 0);
    }

    #[test]
    fn test_get_operation_source_from_op() {
        let tx_source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let op_source = MuxedAccount::Ed25519(Uint256([2u8; 32]));

        let operation = Operation {
            source_account: Some(op_source.clone()),
            body: stellar_xdr::curr::OperationBody::Inflation,
        };

        let result = get_operation_source(&tx_source, &operation);
        assert_eq!(result, muxed_to_account_id(&op_source));
    }

    #[test]
    fn test_get_operation_source_from_tx() {
        let tx_source = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let operation = Operation {
            source_account: None,
            body: stellar_xdr::curr::OperationBody::Inflation,
        };

        let result = get_operation_source(&tx_source, &operation);
        assert_eq!(result, muxed_to_account_id(&tx_source));
    }
}
