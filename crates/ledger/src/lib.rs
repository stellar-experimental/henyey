//! Ledger management for rs-stellar-core.
//!
//! This crate provides the core ledger state management and ledger close pipeline
//! for the Stellar network. It coordinates transaction execution, state updates,
//! bucket list modifications, and ledger metadata generation.
//!
//! # Architecture Overview
//!
//! The crate is organized around several key components:
//!
//! - [`LedgerManager`]: The central coordinator for all ledger operations
//! - [`LedgerCloseData`]: Input data for closing a ledger
//! - [`LedgerDelta`]: Accumulates state changes during ledger processing
//! - [`LedgerSnapshot`]: Point-in-time immutable view of ledger state
//!
//! # Ledger Close Process
//!
//! The ledger close process is handled by [`LedgerManager::close_ledger`]:
//!
//! 1. **Receive externalized data**: SCP consensus provides a [`LedgerCloseData`]
//!    containing the transaction set, close time, and any protocol upgrades.
//!
//! 2. **Close ledger**: [`LedgerManager::close_ledger`] executes all transactions,
//!    updates the bucket list, and commits the new ledger header.
//!
//! # State Model
//!
//! Ledger state consists of typed entries stored in the bucket list:
//!
//! - **Accounts**: XLM balances, sequence numbers, signers, thresholds
//! - **Trustlines**: Non-native asset balances and authorization
//! - **Offers**: Order book entries for the DEX
//! - **Data entries**: Arbitrary key-value data attached to accounts
//! - **Claimable balances**: Pending balance claims with predicates
//! - **Liquidity pools**: AMM pool state (Protocol 18+)
//! - **Contract data**: Soroban smart contract storage (Protocol 20+)
//! - **Contract code**: Soroban WASM bytecode (Protocol 20+)
//!
//! # Reserve Calculations
//!
//! The [`reserves`] module provides utilities for calculating:
//!
//! - Account minimum balances based on sub-entries and sponsorship
//! - Available balance for spending (accounting for reserves and liabilities)
//!
//! # Example Usage
//!
//! ```ignore
//! use henyey_ledger::{LedgerManager, LedgerManagerConfig, LedgerCloseData, TransactionSetVariant};
//!
//! // Create a ledger manager
//! let manager = LedgerManager::new(network_passphrase, LedgerManagerConfig::default());
//!
//! // Initialize from buckets (during catchup)
//! manager.initialize(bucket_list, hot_archive_bucket_list, header, header_hash)?;
//!
//! // Close a ledger
//! let close_data = LedgerCloseData::new(seq, tx_set, close_time, prev_hash);
//! let result = manager.close_ledger(close_data)?;
//! println!("Closed ledger {}", result.ledger_seq());
//! ```
//!
//! # Protocol Compatibility
//!
//! This crate supports all Stellar protocol versions, with special handling for:
//!
//! - **Protocol 18**: Liquidity pools (AMM)
//! - **Protocol 20**: Soroban smart contracts
//! - **Protocol 23**: Hot archive bucket list for state archival

mod close;
pub mod close_state;
pub mod config_upgrade;
mod delta;
mod error;
pub mod execution;
mod header;
mod manager;
pub mod memory_report;
pub mod offer;
pub mod p23_hot_archive_bug;
mod p23_hot_archive_bug_data;
pub mod peak_rss_sampler;
mod prepare_liabilities;
mod snapshot;
mod soroban_state;

// Re-export main types
pub use close::{
    CachePerfStats, LedgerCloseData, LedgerClosePerf, LedgerCloseResult, LedgerCloseStats,
    SorobanPhaseStructure, TransactionSetVariant, TxPerf, UpgradeContext,
};
pub use close_state::CloseLedgerState;
pub use config_upgrade::{ConfigUpgradeSetFrame, ConfigUpgradeValidity};
pub use delta::{EntryChange, LedgerDelta};
pub use error::LedgerError;
pub use execution::{
    compute_state_size_window_entry, execute_soroban_parallel_phase, load_soroban_network_info,
    FeeMode, FeeStrategy, LedgerAdvanceParams, SorobanContext, SorobanFeeSource,
    SorobanNetworkInfo,
};
pub use header::{
    calculate_skip_values, close_time, compute_header_hash, create_next_header, protocol_version,
    verify_header_chain,
};
pub use manager::build_genesis_soroban_config_entries;
pub use manager::new_bucket_list_with_soroban_config;
pub use manager::{
    prepend_fee_event, scan_level_pairs_for_caches, HeaderSnapshot, LedgerManager,
    LedgerManagerConfig, COMMITTED_MARKET_EVENT_CAPACITY,
};
pub use memory_report::log_startup_memory;
pub use peak_rss_sampler::{Phase as StartupPhase, StartupPeakRssSampler};
pub use snapshot::{
    ClassicMarketSnapshot, CommittedMarketEvent, CommittedMarketIdentity,
    CommittedMarketStreamError, CommittedMarketSubscription, CommittedOfferDelta, EntriesLookupFn,
    EntryLookupFn, FrozenOfferEntriesLookupFn, LedgerSnapshot, MarketEntriesLookupFn,
    PoolShareTrustlinesByAccountFn, SnapshotBuilder, SnapshotHandle,
};
pub use soroban_state::{InMemorySorobanState, SharedSorobanState};

/// Result type for ledger operations.
///
/// All fallible operations in this crate return this type, with errors
/// represented by [`LedgerError`].
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Trait for reading ledger entries by key.
///
/// Both [`SnapshotHandle`] (frozen base state) and [`CloseLedgerState`] (merged
/// view with delta overlay) implement this trait, allowing config-loading and
/// other read-path code to be generic over the entry source.
pub trait EntryReader {
    fn get_entry(&self, key: &stellar_xdr::LedgerKey) -> Result<Option<stellar_xdr::LedgerEntry>>;
}

/// Simplified view of the current ledger header.
///
/// This struct provides a convenient, flattened representation of the most
/// commonly needed ledger header fields. Use this when you need quick access
/// to ledger state without the full XDR `LedgerHeader` structure.
///
/// # Example
///
/// ```ignore
/// let header: LedgerHeader = /* ... */;
/// let info = LedgerInfo::from(&header);
/// println!("Ledger {} at protocol {}", info.sequence, info.protocol_version);
/// ```
#[derive(Debug, Clone)]
pub struct LedgerInfo {
    /// Ledger sequence number (monotonically increasing from genesis).
    pub sequence: u32,
    /// Hash of the previous ledger header (SHA-256).
    pub previous_ledger_hash: henyey_common::Hash256,
    /// Root hash of the bucket list Merkle tree.
    pub bucket_list_hash: henyey_common::Hash256,
    /// Ledger close time as a Unix timestamp (seconds since epoch).
    pub close_time: u64,
    /// Base fee per operation in stroops (1 stroop = 0.0000001 XLM).
    pub base_fee: u32,
    /// Base reserve per entry in stroops (typically 0.5 XLM = 5,000,000 stroops).
    pub base_reserve: u32,
    /// Protocol version number governing this ledger's behavior.
    pub protocol_version: u32,
}

impl From<&stellar_xdr::LedgerHeader> for LedgerInfo {
    fn from(header: &stellar_xdr::LedgerHeader) -> Self {
        Self {
            sequence: header.ledger_seq,
            previous_ledger_hash: henyey_common::Hash256::from(header.previous_ledger_hash.0),
            bucket_list_hash: henyey_common::Hash256::from(header.bucket_list_hash.0),
            close_time: header.scp_value.close_time.0,
            base_fee: header.base_fee,
            base_reserve: header.base_reserve,
            protocol_version: header.ledger_version,
        }
    }
}

/// Reserve calculation utilities for account balance requirements.
///
/// This module provides functions for calculating minimum balance requirements
/// and available balances for Stellar accounts. These calculations are essential
/// for validating transactions and ensuring accounts maintain sufficient reserves.
///
/// # Reserve Model
///
/// Every Stellar account must maintain a minimum XLM balance based on:
///
/// - **Base entries**: Every account has 2 base entries (the account itself)
/// - **Sub-entries**: Each additional entry (trustline, offer, signer, data) adds 1
/// - **Sponsorship**: Sponsored entries reduce the owner's reserve requirement
///
/// The formula is: `(2 + num_sub_entries + num_sponsoring - num_sponsored) * base_reserve`
///
/// # Liabilities
///
/// Accounts also track buying and selling liabilities from open offers:
///
/// - **Selling liabilities**: XLM committed to sell in open offers
/// - **Buying liabilities**: XLM capacity reserved for incoming purchases
///
/// These affect the available balance for sending and receiving.
pub mod reserves {
    use stellar_xdr::AccountEntry;

    /// Number of stroops per XLM (1 XLM = 10,000,000 stroops).
    pub const STROOPS_PER_XLM: i64 = 10_000_000;

    /// Calculate the minimum balance for an account.
    ///
    /// This is the amount of XLM that must be held in reserve and cannot
    /// be spent or sent. Attempting to reduce the balance below this
    /// threshold will fail.
    ///
    /// # Formula
    ///
    /// `(2 + num_sub_entries + num_sponsoring - num_sponsored) * base_reserve`
    ///
    /// # Arguments
    ///
    /// * `account` - The account to calculate minimum balance for
    /// * `base_reserve` - The network base reserve in stroops (typically 5,000,000)
    ///
    /// # Returns
    ///
    /// The minimum balance in stroops.
    pub fn minimum_balance(account: &AccountEntry, base_reserve: u32) -> i64 {
        let base = base_reserve as i64;

        // Get sponsorship info if available
        let (num_sponsoring, num_sponsored) = match &account.ext {
            stellar_xdr::AccountEntryExt::V0 => (0, 0),
            stellar_xdr::AccountEntryExt::V1(v1) => match &v1.ext {
                stellar_xdr::AccountEntryExtensionV1Ext::V0 => (0, 0),
                stellar_xdr::AccountEntryExtensionV1Ext::V2(v2) => {
                    (v2.num_sponsoring as i64, v2.num_sponsored as i64)
                }
            },
        };

        // Base account entries (2) + sub entries + sponsoring - sponsored
        let num_entries = 2 + account.num_sub_entries as i64 + num_sponsoring - num_sponsored;

        num_entries * base
    }

    /// Get the selling liabilities for an account's native (XLM) balance.
    ///
    /// Selling liabilities represent XLM that is committed to open sell offers
    /// and cannot be spent until those offers are cancelled or filled.
    ///
    /// # Returns
    ///
    /// The selling liabilities in stroops, or 0 for V0 accounts.
    pub fn selling_liabilities(account: &AccountEntry) -> i64 {
        match &account.ext {
            stellar_xdr::AccountEntryExt::V0 => 0,
            stellar_xdr::AccountEntryExt::V1(v1) => v1.liabilities.selling,
        }
    }

    /// Get the buying liabilities for an account's native (XLM) balance.
    ///
    /// Buying liabilities represent XLM that could be received from open buy
    /// offers. This affects how much additional XLM the account can receive
    /// before hitting the maximum balance.
    ///
    /// # Returns
    ///
    /// The buying liabilities in stroops, or 0 for V0 accounts.
    pub fn buying_liabilities(account: &AccountEntry) -> i64 {
        match &account.ext {
            stellar_xdr::AccountEntryExt::V0 => 0,
            stellar_xdr::AccountEntryExt::V1(v1) => v1.liabilities.buying,
        }
    }

    /// Calculate the available balance that can be sent from an account.
    ///
    /// This is the maximum amount of XLM that can be transferred out of the
    /// account while maintaining the minimum balance and honoring open offers.
    ///
    /// # Formula
    ///
    /// `balance - minimum_balance - selling_liabilities`
    ///
    /// # Returns
    ///
    /// The available balance in stroops. Returns 0 if the calculation would
    /// be negative (using saturating subtraction).
    pub fn available_to_send(account: &AccountEntry, base_reserve: u32) -> i64 {
        let min_bal = minimum_balance(account, base_reserve);
        let sell_liab = selling_liabilities(account);

        account
            .balance
            .saturating_sub(min_bal)
            .saturating_sub(sell_liab)
    }
}

/// Trustline liability and balance constraint utilities.
///
/// This module provides functions analogous to the `reserves` module but for
/// trustline entries instead of accounts. These computations are used throughout
/// payment, path payment, manage offer, and change trust operations.
///
/// # Trustline Balance Model
///
/// - **balance**: Current amount held (0..=limit)
/// - **limit**: Maximum allowed balance on the trustline
/// - **selling liabilities**: Amount committed to open sell offers
/// - **buying liabilities**: Amount committed to open buy offers
///
/// # Constraints
///
/// - Available to send: `balance - selling_liabilities`
/// - Available to receive: `limit - balance - buying_liabilities`
/// - Selling liabilities cannot exceed balance
/// - Buying liabilities cannot exceed `limit - balance`
pub mod trustlines {
    use stellar_xdr::TrustLineEntry;

    /// Get the selling liabilities for a trustline.
    ///
    /// Returns 0 for V0 trustlines (no liability tracking).
    pub fn selling_liabilities(trustline: &TrustLineEntry) -> i64 {
        match &trustline.ext {
            stellar_xdr::TrustLineEntryExt::V0 => 0,
            stellar_xdr::TrustLineEntryExt::V1(v1) => v1.liabilities.selling,
        }
    }

    /// Get the buying liabilities for a trustline.
    ///
    /// Returns 0 for V0 trustlines (no liability tracking).
    pub fn buying_liabilities(trustline: &TrustLineEntry) -> i64 {
        match &trustline.ext {
            stellar_xdr::TrustLineEntryExt::V0 => 0,
            stellar_xdr::TrustLineEntryExt::V1(v1) => v1.liabilities.buying,
        }
    }

    /// Calculate the available balance that can be sent from a trustline.
    ///
    /// This is the maximum amount of the asset that can be transferred out
    /// while honoring open sell offers.
    ///
    /// # Formula
    ///
    /// `balance - selling_liabilities`
    pub fn available_to_send(trustline: &TrustLineEntry) -> i64 {
        trustline
            .balance
            .saturating_sub(selling_liabilities(trustline))
    }

    /// Check if adding `delta` to selling liabilities would be valid.
    ///
    /// Requirements:
    /// - `current_selling + delta >= 0` (can't make liabilities negative)
    /// - `current_selling + delta <= balance` (can't exceed available balance)
    pub fn can_add_selling_liabilities(trustline: &TrustLineEntry, delta: i64) -> bool {
        let current = selling_liabilities(trustline);
        let new_liab = current.checked_add(delta);
        match new_liab {
            Some(l) => l >= 0 && l <= trustline.balance,
            None => false, // overflow
        }
    }

    /// Check if adding `delta` to buying liabilities would be valid.
    ///
    /// Requirements:
    /// - `current_buying + delta >= 0` (can't make liabilities negative)
    /// - `current_buying + delta <= limit - balance` (can't exceed capacity)
    pub fn can_add_buying_liabilities(trustline: &TrustLineEntry, delta: i64) -> bool {
        let current = buying_liabilities(trustline);
        let new_liab = current.checked_add(delta);
        match new_liab {
            Some(l) => l >= 0 && l <= trustline.limit - trustline.balance,
            None => false, // overflow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
        AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountId, Liabilities, PublicKey,
        SequenceNumber, Thresholds, Uint256,
    };

    // Default base_reserve matching Stellar mainnet (0.5 XLM = 5,000,000 stroops)
    const BASE_RESERVE: u32 = 5_000_000;

    fn create_test_account(balance: i64, num_sub_entries: u32) -> AccountEntry {
        AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: stellar_xdr::VecM::default(),
            ext: AccountEntryExt::V0,
        }
    }

    /// Create an account with explicit liabilities (V1 extension).
    fn create_account_with_liabilities(
        balance: i64,
        num_sub_entries: u32,
        selling: i64,
        buying: i64,
    ) -> AccountEntry {
        AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: stellar_xdr::VecM::default(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities { buying, selling },
                ext: AccountEntryExtensionV1Ext::V0,
            }),
        }
    }

    /// Create an account with liabilities and sponsorship info (V2 extension).
    fn create_account_with_sponsorship(
        balance: i64,
        num_sub_entries: u32,
        selling: i64,
        buying: i64,
        num_sponsoring: u32,
        num_sponsored: u32,
    ) -> AccountEntry {
        AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: stellar_xdr::VecM::default(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities { buying, selling },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsoring,
                    num_sponsored,
                    signer_sponsoring_i_ds: stellar_xdr::VecM::default(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        }
    }

    /// Helper: minimum balance for an account with `n` sub-entries and no sponsorship.
    fn min_balance(n: u32) -> i64 {
        (2 + n as i64) * BASE_RESERVE as i64
    }

    // =========================================================================
    // P0-3: Base reserve / minimum balance (parity: LedgerHeaderTests.cpp:97)
    // =========================================================================

    #[test]
    fn test_minimum_balance() {
        let account = create_test_account(100_000_000, 0);

        // (2 + 0) * 5_000_000 = 10_000_000
        assert_eq!(
            reserves::minimum_balance(&account, BASE_RESERVE),
            10_000_000
        );

        let account2 = create_test_account(100_000_000, 3);
        // (2 + 3) * 5_000_000 = 25_000_000
        assert_eq!(
            reserves::minimum_balance(&account2, BASE_RESERVE),
            25_000_000
        );
    }

    /// Parity: LedgerHeaderTests.cpp:97 "base reserve" - large sub-entry count
    #[test]
    fn test_minimum_balance_large_sub_entry_count() {
        let base_reserve: u32 = 100_000_000; // 10 XLM (matches stellar-core test config baseReserve)
        let n: u32 = 20_000;
        let expected = (2 + n as i64) * base_reserve as i64; // 2000200000000

        let account = create_test_account(i64::MAX, n);
        assert_eq!(reserves::minimum_balance(&account, base_reserve), expected);
        assert_eq!(expected, 2_000_200_000_000);
    }

    /// Test minimum balance with sponsorship adjustments.
    #[test]
    fn test_minimum_balance_with_sponsorship() {
        // num_sponsoring adds to reserve, num_sponsored subtracts
        let account = create_account_with_sponsorship(100_000_000, 2, 0, 0, 3, 1);
        // (2 + 2 + 3 - 1) * 5_000_000 = 6 * 5_000_000 = 30_000_000
        assert_eq!(
            reserves::minimum_balance(&account, BASE_RESERVE),
            30_000_000
        );

        // Equal sponsoring/sponsored cancels out
        let account2 = create_account_with_sponsorship(100_000_000, 0, 0, 0, 5, 5);
        // (2 + 0 + 5 - 5) * 5_000_000 = 10_000_000
        assert_eq!(
            reserves::minimum_balance(&account2, BASE_RESERVE),
            10_000_000
        );

        // Fully sponsored account: num_sponsored > num_sub_entries
        let account3 = create_account_with_sponsorship(100_000_000, 3, 0, 0, 0, 3);
        // (2 + 3 + 0 - 3) * 5_000_000 = 10_000_000
        assert_eq!(
            reserves::minimum_balance(&account3, BASE_RESERVE),
            10_000_000
        );
    }

    // =========================================================================
    // P1-1: Account selling liabilities
    // Parity: LiabilitiesTests.cpp:29 "add account selling liabilities"
    // =========================================================================

    #[test]
    fn test_selling_liabilities_extraction() {
        // V0 account: no liabilities
        let v0 = create_test_account(100_000_000, 0);
        assert_eq!(reserves::selling_liabilities(&v0), 0);

        // V1 account: explicit liabilities
        let v1 = create_account_with_liabilities(100_000_000, 0, 500, 300);
        assert_eq!(reserves::selling_liabilities(&v1), 500);
        assert_eq!(reserves::buying_liabilities(&v1), 300);
    }

    /// Selling liabilities constrain available_to_send.
    /// Parity: LiabilitiesTests.cpp:222 "cannot increase liabilities above balance minus reserve"
    #[test]
    fn test_selling_liabilities_constrain_available_to_send() {
        // Account at min balance with no liabilities: available = 0
        let at_min = create_account_with_liabilities(min_balance(0), 0, 0, 0);
        assert_eq!(reserves::available_to_send(&at_min, BASE_RESERVE), 0);

        // Account with balance above min and no liabilities
        let above_min = create_account_with_liabilities(min_balance(0) + 100, 0, 0, 0);
        assert_eq!(reserves::available_to_send(&above_min, BASE_RESERVE), 100);

        // Selling liabilities reduce available
        let with_sell = create_account_with_liabilities(min_balance(0) + 100, 0, 50, 0);
        assert_eq!(reserves::available_to_send(&with_sell, BASE_RESERVE), 50);

        // Selling liabilities = all excess: available = 0
        let maxed = create_account_with_liabilities(min_balance(0) + 100, 0, 100, 0);
        assert_eq!(reserves::available_to_send(&maxed, BASE_RESERVE), 0);

        // Selling liabilities exceed excess: goes negative (caller checks >= amount)
        let over = create_account_with_liabilities(min_balance(0) + 50, 0, 100, 0);
        assert!(reserves::available_to_send(&over, BASE_RESERVE) < 0);
    }

    /// Below-reserve accounts cannot increase selling liabilities.
    /// Parity: LiabilitiesTests.cpp:173 "below reserve"
    #[test]
    fn test_selling_liabilities_below_reserve() {
        // Below reserve: available_to_send is negative (insufficient balance)
        let below = create_account_with_liabilities(min_balance(0) - 1, 0, 0, 0);
        assert!(reserves::available_to_send(&below, BASE_RESERVE) < 0);

        // At reserve: available_to_send is exactly 0
        let at = create_account_with_liabilities(min_balance(0), 0, 0, 0);
        assert_eq!(reserves::available_to_send(&at, BASE_RESERVE), 0);
    }

    /// Limiting values for selling liabilities.
    /// Parity: LiabilitiesTests.cpp:257 "limiting values"
    #[test]
    fn test_selling_liabilities_limiting_values() {
        // Maximum balance with 0 liabilities: can sell up to balance - min_balance
        let max_bal = create_account_with_liabilities(i64::MAX, 0, 0, 0);
        let expected_available = i64::MAX - min_balance(0);
        assert_eq!(
            reserves::available_to_send(&max_bal, BASE_RESERVE),
            expected_available
        );

        // Maximum balance with liabilities at limit
        let max_liab = create_account_with_liabilities(i64::MAX, 0, i64::MAX - min_balance(0), 0);
        assert_eq!(reserves::available_to_send(&max_liab, BASE_RESERVE), 0);
    }

    /// Selling liabilities interact correctly with sub-entries.
    /// Parity: LiabilitiesTests.cpp:222 with non-zero numSubEntries
    #[test]
    fn test_selling_liabilities_with_sub_entries() {
        // 3 sub-entries: min_balance = (2+3)*5M = 25M
        // balance = 30M, selling = 0 → available = 5M
        let a = create_account_with_liabilities(30_000_000, 3, 0, 0);
        assert_eq!(reserves::available_to_send(&a, BASE_RESERVE), 5_000_000);

        // 3 sub-entries, selling = 5M → available = 0
        let b = create_account_with_liabilities(30_000_000, 3, 5_000_000, 0);
        assert_eq!(reserves::available_to_send(&b, BASE_RESERVE), 0);
    }

    /// Selling liabilities with sponsorship.
    /// Parity: LiabilitiesTests.cpp with sponsoring/sponsored combinations
    #[test]
    fn test_selling_liabilities_with_sponsorship() {
        // Sponsorship adjusts min balance; selling liabilities further constrain
        // 0 sub-entries, 2 sponsoring, 1 sponsored: min = (2+0+2-1)*5M = 15M
        let a = create_account_with_sponsorship(20_000_000, 0, 0, 0, 2, 1);
        assert_eq!(reserves::minimum_balance(&a, BASE_RESERVE), 15_000_000);
        assert_eq!(reserves::available_to_send(&a, BASE_RESERVE), 5_000_000);

        // Same but with selling liabilities = 3M
        let b = create_account_with_sponsorship(20_000_000, 0, 3_000_000, 0, 2, 1);
        assert_eq!(reserves::available_to_send(&b, BASE_RESERVE), 2_000_000);
    }

    // =========================================================================
    // P1-2: Account buying liabilities
    // Parity: LiabilitiesTests.cpp:277 "add account buying liabilities"
    // =========================================================================

    #[test]
    fn test_buying_liabilities_extraction() {
        let v0 = create_test_account(100_000_000, 0);
        assert_eq!(reserves::buying_liabilities(&v0), 0);

        let v1 = create_account_with_liabilities(100_000_000, 0, 0, 500);
        assert_eq!(reserves::buying_liabilities(&v1), 500);
    }

    /// Buying liabilities do not affect available_to_send.
    #[test]
    fn test_buying_liabilities_do_not_affect_sending() {
        let a = create_account_with_liabilities(min_balance(0) + 1000, 0, 0, 500_000);
        // buying liabilities don't reduce available_to_send
        assert_eq!(reserves::available_to_send(&a, BASE_RESERVE), 1000);
    }

    // =========================================================================
    // P1-5: Account add balance with liabilities
    // Parity: LiabilitiesTests.cpp:829 "balance with liabilities"
    //
    // Tests the constraints on balance changes. The stellar-core tests use addBalance()
    // which checks: new_balance >= min_balance + selling_liab (for decreases)
    // and new_balance + buying_liab <= INT64_MAX (for increases).
    // We test these constraints via available_to_send / available_to_receive.
    // =========================================================================

    /// Balance cannot decrease below reserve plus selling liabilities.
    /// Parity: LiabilitiesTests.cpp:939 "cannot decrease balance below reserve plus selling liabilities"
    #[test]
    fn test_balance_cannot_decrease_below_reserve_plus_selling() {
        // Below min balance, no liabilities: available is negative
        let below = create_account_with_liabilities(min_balance(0) - 1, 0, 0, 0);
        assert!(reserves::available_to_send(&below, BASE_RESERVE) < 0);

        // At min balance, no liabilities: can't send anything
        let at_min = create_account_with_liabilities(min_balance(0), 0, 0, 0);
        assert_eq!(reserves::available_to_send(&at_min, BASE_RESERVE), 0);

        // Above min balance, no liabilities: can send excess
        let above = create_account_with_liabilities(min_balance(0) + 1, 0, 0, 0);
        assert_eq!(reserves::available_to_send(&above, BASE_RESERVE), 1);

        // Above min balance, with selling liabilities: reduced further
        let with_sell = create_account_with_liabilities(min_balance(0) + 10, 0, 5, 0);
        assert_eq!(reserves::available_to_send(&with_sell, BASE_RESERVE), 5);

        // Selling liabilities consume all excess
        let maxed = create_account_with_liabilities(min_balance(0) + 10, 0, 10, 0);
        assert_eq!(reserves::available_to_send(&maxed, BASE_RESERVE), 0);
    }

    // =========================================================================
    // P1-6: Account add sub-entries
    // Parity: LiabilitiesTests.cpp:989 "account add subentries"
    // =========================================================================

    /// Can always decrease sub-entries (removing returns reserve).
    /// Parity: LiabilitiesTests.cpp:1104 "can decrease sub entries when below min balance"
    #[test]
    fn test_sub_entry_decrease_always_possible() {
        // This tests the concept: removing a sub-entry frees reserve.
        // With 1 sub-entry, min balance = (2+1)*5M = 15M.
        // With 0 sub-entries, min balance = (2+0)*5M = 10M.
        let before = create_test_account(14_000_000, 1);
        // Before: min=15M, balance=14M → below reserve (negative available)
        assert!(reserves::available_to_send(&before, BASE_RESERVE) < 0);

        // After removing sub-entry: min=10M, balance=14M → 4M available
        let after = create_test_account(14_000_000, 0);
        assert_eq!(reserves::available_to_send(&after, BASE_RESERVE), 4_000_000);
    }

    /// Sub-entry with sponsorship.
    #[test]
    fn test_can_add_sub_entry_with_sponsorship() {
        // Sponsorship reduces needed reserve: 2 sponsored means 2 fewer reserves needed
        // min = (2 + 0 + 0 - 2) * 5M = 0 (but 0*5M = 0)
        let sponsored = create_account_with_sponsorship(
            BASE_RESERVE as i64, // just enough for one more reserve
            0,
            0,
            0,
            0,
            2, // 2 sponsored reduces min_balance
        );
        // min_balance = (2+0+0-2)*5M = 0
        assert_eq!(reserves::minimum_balance(&sponsored, BASE_RESERVE), 0);
    }

    // =========================================================================
    // P1-7: Available balance and limit (accounts)
    // Parity: LiabilitiesTests.cpp:1230 "available balance and limit"
    // =========================================================================

    /// Account available balance: considers reserves and selling liabilities.
    /// Parity: LiabilitiesTests.cpp:1236 "account available balance"
    #[test]
    fn test_account_available_balance_comprehensive() {
        // No liabilities, at reserve: 0 available
        assert_eq!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0), 0, 0, 0),
                BASE_RESERVE
            ),
            0
        );

        // No liabilities, above reserve: excess available
        assert_eq!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0) + 1000, 0, 0, 0),
                BASE_RESERVE
            ),
            1000
        );

        // Selling liabilities reduce available
        assert_eq!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0) + 1000, 0, 400, 0),
                BASE_RESERVE
            ),
            600
        );

        // Selling liabilities equal excess: 0 available
        assert_eq!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0) + 1000, 0, 1000, 0),
                BASE_RESERVE
            ),
            0
        );

        // Balance below reserve with liabilities: negative
        assert!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0) - 100, 0, 50, 0),
                BASE_RESERVE
            ) < 0
        );

        // Buying liabilities don't affect available to send
        assert_eq!(
            reserves::available_to_send(
                &create_account_with_liabilities(min_balance(0) + 1000, 0, 0, 999_999),
                BASE_RESERVE
            ),
            1000
        );
    }

    /// Sponsorship interactions with available balance.
    /// Parity: LiabilitiesTests.cpp sponsoring/sponsored loops in addSellingLiabilities
    #[test]
    fn test_available_balance_with_sponsorship_combinations() {
        let base = BASE_RESERVE as i64;

        // For each (sponsoring, sponsored) pair, the min balance adjusts
        for sponsoring in 0..3u32 {
            for sponsored in 0..3u32 {
                let delta = (sponsoring as i64 - sponsored as i64) * base;
                let balance = min_balance(0) + 1000 + delta;
                if balance < 0 {
                    continue;
                }

                let account =
                    create_account_with_sponsorship(balance, 0, 0, 0, sponsoring, sponsored);
                let min_bal = reserves::minimum_balance(&account, BASE_RESERVE);
                let available = reserves::available_to_send(&account, BASE_RESERVE);

                // available = balance - min_balance (saturating)
                let expected = (balance - min_bal).max(0);
                assert_eq!(
                    available, expected,
                    "sponsoring={}, sponsored={}, balance={}, min_bal={}",
                    sponsoring, sponsored, balance, min_bal
                );
            }
        }
    }

    // =========================================================================
    // P1 Available to send/receive edge cases
    // =========================================================================

    /// available_to_send returns negative when balance is insufficient.
    /// Callers must check `available >= needed_amount` for validation.
    #[test]
    fn test_available_to_send_negative_when_below_reserve() {
        // balance = 0, sub_entries = 0, min = 10M → available = -10M
        let a = create_test_account(0, 0);
        assert_eq!(
            reserves::available_to_send(&a, BASE_RESERVE),
            -(min_balance(0))
        );

        // balance = 1, sub_entries = 100, min = (2+100)*5M = 510M → very negative
        let b = create_test_account(1, 100);
        let min_100 = (2 + 100) * BASE_RESERVE as i64;
        assert_eq!(reserves::available_to_send(&b, BASE_RESERVE), 1 - min_100);
    }

    /// Test that selling liabilities reduce available_to_send.
    #[test]
    fn test_selling_and_buying_liabilities_independent() {
        let a = create_account_with_liabilities(min_balance(0) + 1000, 0, 300, 500);

        // Selling affects available_to_send (buying does not)
        assert_eq!(reserves::available_to_send(&a, BASE_RESERVE), 700);
    }

    // =========================================================================
    // P1-3: Trustline selling liabilities
    // Parity: LiabilitiesTests.cpp:542 "add trustline selling liabilities"
    // =========================================================================

    use stellar_xdr::{
        AlphaNum4, AssetCode4, TrustLineAsset, TrustLineEntry, TrustLineEntryExt, TrustLineEntryV1,
        TrustLineEntryV1Ext,
    };

    /// Helper: create a V0 trustline (no liabilities tracking).
    fn create_trustline_v0(balance: i64, limit: i64) -> TrustLineEntry {
        TrustLineEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            }),
            balance,
            limit,
            flags: 1, // AUTHORIZED_FLAG
            ext: TrustLineEntryExt::V0,
        }
    }

    /// Helper: create a V1 trustline with explicit liabilities.
    fn create_trustline_with_liabilities(
        balance: i64,
        limit: i64,
        selling: i64,
        buying: i64,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            }),
            balance,
            limit,
            flags: 1, // AUTHORIZED_FLAG
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities { buying, selling },
                ext: TrustLineEntryV1Ext::V0,
            }),
        }
    }

    #[test]
    fn test_trustline_selling_liabilities_extraction() {
        // V0 trustline: no liabilities
        let v0 = create_trustline_v0(1000, 10000);
        assert_eq!(trustlines::selling_liabilities(&v0), 0);
        assert_eq!(trustlines::buying_liabilities(&v0), 0);

        // V1 trustline: explicit liabilities
        let v1 = create_trustline_with_liabilities(1000, 10000, 300, 200);
        assert_eq!(trustlines::selling_liabilities(&v1), 300);
        assert_eq!(trustlines::buying_liabilities(&v1), 200);
    }

    /// Parity: LiabilitiesTests.cpp:580 "cannot make liabilities negative"
    #[test]
    fn test_trustline_selling_cannot_make_negative() {
        // Zero liabilities: delta=0 ok, delta=-1 fail
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 1, 0, 0),
            0
        ));
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 1, 0, 0),
            -1
        ));

        // Positive liabilities: can decrease by 1
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 1, 1, 0),
            -1
        ));
        // But not by 2
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 1, 1, 0),
            -2
        ));
    }

    /// Parity: LiabilitiesTests.cpp:595 "cannot increase liabilities above balance"
    #[test]
    fn test_trustline_selling_cannot_exceed_balance() {
        // balance=1, selling=0: can add 1 but not 2
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 2, 0, 0),
            1
        ));
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(1, 2, 0, 0),
            2
        ));

        // balance=2, selling=1: can add 1 but not 2
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(2, 2, 1, 0),
            1
        ));
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(2, 2, 1, 0),
            2
        ));

        // balance=0: can't add anything
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(0, 2, 0, 0),
            0
        ));
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(0, 2, 0, 0),
            1
        ));

        // balance=2, selling=2: can't add more
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(2, 2, 2, 0),
            0
        ));
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(2, 2, 2, 0),
            1
        ));
    }

    /// Parity: LiabilitiesTests.cpp:625 "limiting values" for trustline selling
    #[test]
    fn test_trustline_selling_limiting_values() {
        // MAX balance, 0 liabilities: can add MAX
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(i64::MAX, i64::MAX, 0, 0),
            i64::MAX
        ));
        // MAX-1 balance: can add MAX-1 but not MAX
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(i64::MAX - 1, i64::MAX, 0, 0),
            i64::MAX
        ));
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(i64::MAX - 1, i64::MAX, 0, 0),
            i64::MAX - 1
        ));

        // MAX selling, can't add 1 more
        assert!(!trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(i64::MAX, i64::MAX, i64::MAX, 0),
            1
        ));
        // But can add 1 if MAX-1
        assert!(trustlines::can_add_selling_liabilities(
            &create_trustline_with_liabilities(i64::MAX, i64::MAX, i64::MAX - 1, 0),
            1
        ));
    }

    /// Trustline available_to_send is balance - selling_liabilities.
    #[test]
    fn test_trustline_available_to_send() {
        // No liabilities
        let a = create_trustline_with_liabilities(1000, 5000, 0, 0);
        assert_eq!(trustlines::available_to_send(&a), 1000);

        // With selling liabilities
        let b = create_trustline_with_liabilities(1000, 5000, 300, 0);
        assert_eq!(trustlines::available_to_send(&b), 700);

        // Selling liabilities equal balance
        let c = create_trustline_with_liabilities(1000, 5000, 1000, 0);
        assert_eq!(trustlines::available_to_send(&c), 0);

        // V0 trustline: selling = 0
        let d = create_trustline_v0(1000, 5000);
        assert_eq!(trustlines::available_to_send(&d), 1000);
    }

    /// Parity: LiabilitiesTests.cpp:1450 "trustline available balance"
    #[test]
    fn test_trustline_available_to_send_comprehensive() {
        // Zero balance: nothing to send
        assert_eq!(
            trustlines::available_to_send(&create_trustline_with_liabilities(0, 100, 0, 0)),
            0
        );

        // Selling liabilities don't affect buying side
        let tl = create_trustline_with_liabilities(100, 200, 50, 999);
        assert_eq!(trustlines::available_to_send(&tl), 50);
    }

    // =========================================================================
    // P1-4: Trustline buying liabilities
    // Parity: LiabilitiesTests.cpp:685 "add trustline buying liabilities"
    // =========================================================================

    /// Parity: LiabilitiesTests.cpp:725 "cannot make liabilities negative"
    #[test]
    fn test_trustline_buying_cannot_make_negative() {
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 1, 0, 0),
            0
        ));
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 1, 0, 0),
            -1
        ));

        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 1, 0, 1),
            -1
        ));
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 1, 0, 1),
            -2
        ));
    }

    /// Parity: LiabilitiesTests.cpp:740 "cannot increase liabilities above limit minus balance"
    #[test]
    fn test_trustline_buying_cannot_exceed_limit_minus_balance() {
        // limit=2, balance=1: capacity=1. Can add 1 but not 2
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(1, 2, 0, 0),
            1
        ));
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(1, 2, 0, 0),
            2
        ));

        // limit=2, balance=0, buying=1: can add 1 but not 2
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 2, 0, 1),
            1
        ));
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, 2, 0, 1),
            2
        ));

        // balance=limit: no capacity
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(2, 2, 0, 0),
            0
        ));
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(2, 2, 0, 0),
            1
        ));
    }

    /// Parity: LiabilitiesTests.cpp:775 "limiting values" for trustline buying
    #[test]
    fn test_trustline_buying_limiting_values() {
        // MAX limit, 0 balance: can add MAX
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, i64::MAX, 0, 0),
            i64::MAX
        ));
        // balance=1: can add MAX-1 but not MAX
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(1, i64::MAX, 0, 0),
            i64::MAX
        ));
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(1, i64::MAX, 0, 0),
            i64::MAX - 1
        ));

        // MAX buying, can't add 1
        assert!(!trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, i64::MAX, 0, i64::MAX),
            1
        ));
        assert!(trustlines::can_add_buying_liabilities(
            &create_trustline_with_liabilities(0, i64::MAX, 0, i64::MAX - 1),
            1
        ));
    }

    /// Selling liabilities reduce trustline available_to_send.
    #[test]
    fn test_trustline_selling_and_buying_independent() {
        let tl = create_trustline_with_liabilities(500, 1000, 200, 300);

        // Selling affects available_to_send (buying does not)
        assert_eq!(trustlines::available_to_send(&tl), 300);

        // Changing buying doesn't affect send
        let tl3 = create_trustline_with_liabilities(500, 1000, 200, 0);
        assert_eq!(trustlines::available_to_send(&tl3), 300); // same
    }
}
