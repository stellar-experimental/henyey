//! Flow control constants and helpers for transaction size limits.
//!
//! This module provides constants and helper functions for flow control
//! in the overlay network, matching the stellar-core implementation in
//! `Herder.h` and `Peer.h`.
//!
//! # Overview
//!
//! Flow control manages the size of transactions that can be sent through
//! the overlay network. The key constants are:
//!
//! - [`MAX_CLASSIC_TX_SIZE_BYTES`]: Maximum size of a classic (non-Soroban) transaction
//! - [`FLOW_CONTROL_BYTES_EXTRA_BUFFER`]: Extra buffer added to Soroban tx size limits
//!
//! The effective maximum transaction size is computed dynamically based on
//! protocol version and Soroban network configuration.

/// Maximum size in bytes for a classic (non-Soroban) transaction.
///
/// This is a static limit that doesn't change with protocol upgrades.
/// It matches `MAX_CLASSIC_TX_SIZE_BYTES` in stellar-core (`Peer.h`).
pub const MAX_CLASSIC_TX_SIZE_BYTES: u32 = 100 * 1024; // 100 KB

/// Extra buffer added to Soroban transaction size limits.
///
/// When computing the maximum transaction size for Soroban transactions,
/// this buffer is added on top of the `txMaxSizeBytes` from the network
/// config to account for envelope overhead.
///
/// Matches `FLOW_CONTROL_BYTES_EXTRA_BUFFER` in stellar-core (`Herder.cpp`).
pub const FLOW_CONTROL_BYTES_EXTRA_BUFFER: u32 = 2000;

/// Default multiplier for computing reading capacity from max transaction size.
pub const DEFAULT_READING_CAPACITY_MULTIPLIER: u32 = 300;

/// Default number of transactions to request in each send-more batch.
pub const DEFAULT_SEND_MORE_BATCH_COUNT: u32 = 40;

/// Compute the maximum transaction size.
///
/// For protocols without Soroban, returns [`MAX_CLASSIC_TX_SIZE_BYTES`].
/// For Soroban-enabled protocols, returns the maximum of the classic limit
/// and the Soroban limit (txMaxSizeBytes + extra buffer).
///
/// # Arguments
///
/// * `protocol_version` - Current protocol version
/// * `soroban_tx_max_size_bytes` - Optional Soroban network config txMaxSizeBytes
///
/// # Returns
///
/// The maximum transaction size in bytes.
///
/// # Example
///
/// ```
/// use henyey_herder::flow_control::compute_max_tx_size;
///
/// // Pre-Soroban protocol
/// assert_eq!(compute_max_tx_size(19, None), 100 * 1024);
///
/// // Soroban protocol with large txMaxSizeBytes
/// let max_size = compute_max_tx_size(25, Some(500_000));
/// assert_eq!(max_size, 500_000 + 2000);
/// ```
pub fn compute_max_tx_size(protocol_version: u32, soroban_tx_max_size_bytes: Option<u32>) -> u32 {
    const SOROBAN_PROTOCOL_VERSION: u32 = 20;

    if protocol_version >= SOROBAN_PROTOCOL_VERSION {
        if let Some(soroban_max) = soroban_tx_max_size_bytes {
            let soroban_limit = soroban_max.saturating_add(FLOW_CONTROL_BYTES_EXTRA_BUFFER);
            return std::cmp::max(MAX_CLASSIC_TX_SIZE_BYTES, soroban_limit);
        }
    }

    MAX_CLASSIC_TX_SIZE_BYTES
}

/// Check if a transaction size exceeds the maximum allowed.
///
/// # Arguments
///
/// * `tx_size` - Size of the transaction in bytes
/// * `max_tx_size` - Maximum allowed size (from [`compute_max_tx_size`])
///
/// # Returns
///
/// `true` if the transaction is too large.
pub fn is_tx_too_large(tx_size: u32, max_tx_size: u32) -> bool {
    tx_size > max_tx_size
}

/// Compute flow control reading capacity.
///
/// This determines how many bytes of transactions a peer is willing to
/// buffer for reading. If not configured, it's computed based on max tx size.
///
/// # Arguments
///
/// * `configured_capacity` - Configured capacity (0 means use default)
/// * `max_tx_size` - Maximum transaction size
/// * `multiplier` - Number of transactions worth of buffer (default 300)
///
/// # Returns
///
/// The reading capacity in bytes.
pub fn compute_reading_capacity(
    configured_capacity: u32,
    max_tx_size: u32,
    multiplier: u32,
) -> u32 {
    if configured_capacity > 0 {
        configured_capacity
    } else {
        max_tx_size.saturating_mul(multiplier)
    }
}

/// Compute flow control "send more" batch size.
///
/// This determines how many bytes of transactions to request when
/// asking a peer for more data.
///
/// # Arguments
///
/// * `configured_batch_size` - Configured batch size (0 means use default)
/// * `max_tx_size` - Maximum transaction size
/// * `batch_count` - Number of transactions per batch (default 40)
///
/// # Returns
///
/// The batch size in bytes.
pub fn compute_send_more_batch_size(
    configured_batch_size: u32,
    max_tx_size: u32,
    batch_count: u32,
) -> u32 {
    if configured_batch_size > 0 {
        configured_batch_size
    } else {
        max_tx_size.saturating_mul(batch_count)
    }
}

/// Flow control configuration computed from network state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowControlConfig {
    /// Maximum transaction size in bytes.
    pub max_tx_size: u32,
    /// Maximum classic transaction size in bytes.
    pub max_classic_tx_size: u32,
    /// Extra buffer for Soroban transactions.
    pub extra_buffer: u32,
    /// Reading capacity in bytes.
    pub reading_capacity: u32,
    /// Send-more batch size in bytes.
    pub send_more_batch_size: u32,
}

impl Default for FlowControlConfig {
    fn default() -> Self {
        Self {
            max_tx_size: MAX_CLASSIC_TX_SIZE_BYTES,
            max_classic_tx_size: MAX_CLASSIC_TX_SIZE_BYTES,
            extra_buffer: FLOW_CONTROL_BYTES_EXTRA_BUFFER,
            reading_capacity: MAX_CLASSIC_TX_SIZE_BYTES * DEFAULT_READING_CAPACITY_MULTIPLIER,
            send_more_batch_size: MAX_CLASSIC_TX_SIZE_BYTES * DEFAULT_SEND_MORE_BATCH_COUNT,
        }
    }
}

impl FlowControlConfig {
    /// Create a new flow control configuration.
    ///
    /// # Arguments
    ///
    /// * `protocol_version` - Current protocol version
    /// * `soroban_tx_max_size_bytes` - Optional Soroban network config
    /// * `configured_reading_capacity` - Configured reading capacity (0 = default)
    /// * `configured_batch_size` - Configured batch size (0 = default)
    pub fn new(
        protocol_version: u32,
        soroban_tx_max_size_bytes: Option<u32>,
        configured_reading_capacity: u32,
        configured_batch_size: u32,
    ) -> Self {
        let max_tx_size = compute_max_tx_size(protocol_version, soroban_tx_max_size_bytes);
        let reading_capacity = compute_reading_capacity(
            configured_reading_capacity,
            max_tx_size,
            DEFAULT_READING_CAPACITY_MULTIPLIER,
        );
        let send_more_batch_size = compute_send_more_batch_size(
            configured_batch_size,
            max_tx_size,
            DEFAULT_SEND_MORE_BATCH_COUNT,
        );

        Self {
            max_tx_size,
            max_classic_tx_size: MAX_CLASSIC_TX_SIZE_BYTES,
            extra_buffer: FLOW_CONTROL_BYTES_EXTRA_BUFFER,
            reading_capacity,
            send_more_batch_size,
        }
    }

    /// Update the configuration after a Soroban config upgrade.
    ///
    /// Returns the increase in max tx size (or 0 if unchanged/decreased).
    pub fn update_for_soroban(&mut self, new_tx_max_size_bytes: u32) -> u32 {
        let new_max = new_tx_max_size_bytes.saturating_add(self.extra_buffer);
        let new_max = std::cmp::max(self.max_classic_tx_size, new_max);

        // Only report an increase for flow control capacity adjustment
        let diff = if new_max > self.max_tx_size {
            new_max - self.max_tx_size
        } else {
            0
        };

        // Always update max_tx_size to reflect the current config, including
        // decreases. Matches stellar-core HerderImpl::maybeHandleUpgrade():
        //   mMaxTxSize = std::max(getMaxClassicTxSize(), maybeNewMaxTxSize);
        self.max_tx_size = new_max;

        diff
    }

    /// Check if a transaction is within size limits.
    pub fn is_tx_size_valid(&self, tx_size: u32) -> bool {
        tx_size <= self.max_tx_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(MAX_CLASSIC_TX_SIZE_BYTES, 100 * 1024);
        assert_eq!(FLOW_CONTROL_BYTES_EXTRA_BUFFER, 2000);
    }

    #[test]
    fn test_compute_max_tx_size_pre_soroban() {
        // Protocol 19 (pre-Soroban)
        assert_eq!(compute_max_tx_size(19, None), MAX_CLASSIC_TX_SIZE_BYTES);
        assert_eq!(
            compute_max_tx_size(19, Some(500_000)),
            MAX_CLASSIC_TX_SIZE_BYTES
        );
    }

    #[test]
    fn test_compute_max_tx_size_soroban() {
        // Protocol 20+ (Soroban)
        // When Soroban limit is larger
        let soroban_max = 500_000;
        let expected = soroban_max + FLOW_CONTROL_BYTES_EXTRA_BUFFER;
        assert_eq!(compute_max_tx_size(20, Some(soroban_max)), expected);
        assert_eq!(compute_max_tx_size(25, Some(soroban_max)), expected);

        // When classic limit is larger (small Soroban config)
        let small_soroban = 10_000;
        assert_eq!(
            compute_max_tx_size(20, Some(small_soroban)),
            MAX_CLASSIC_TX_SIZE_BYTES
        );
    }

    #[test]
    fn test_compute_max_tx_size_no_soroban_config() {
        // Soroban protocol but no config yet
        assert_eq!(compute_max_tx_size(20, None), MAX_CLASSIC_TX_SIZE_BYTES);
    }

    #[test]
    fn test_is_tx_too_large() {
        let max = 100_000;
        assert!(!is_tx_too_large(50_000, max));
        assert!(!is_tx_too_large(100_000, max));
        assert!(is_tx_too_large(100_001, max));
    }

    #[test]
    fn test_compute_reading_capacity() {
        let max_tx = 100_000;

        // Configured value takes precedence
        assert_eq!(compute_reading_capacity(5_000_000, max_tx, 300), 5_000_000);

        // Default calculation
        assert_eq!(compute_reading_capacity(0, max_tx, 300), 100_000 * 300);
    }

    #[test]
    fn test_compute_send_more_batch_size() {
        let max_tx = 100_000;

        // Configured value takes precedence
        assert_eq!(
            compute_send_more_batch_size(2_000_000, max_tx, 40),
            2_000_000
        );

        // Default calculation
        assert_eq!(compute_send_more_batch_size(0, max_tx, 40), 100_000 * 40);
    }

    #[test]
    fn test_flow_control_config_default() {
        let config = FlowControlConfig::default();
        assert_eq!(config.max_tx_size, MAX_CLASSIC_TX_SIZE_BYTES);
        assert_eq!(config.max_classic_tx_size, MAX_CLASSIC_TX_SIZE_BYTES);
        assert_eq!(config.extra_buffer, FLOW_CONTROL_BYTES_EXTRA_BUFFER);
    }

    #[test]
    fn test_flow_control_config_new() {
        // Pre-Soroban
        let config = FlowControlConfig::new(19, None, 0, 0);
        assert_eq!(config.max_tx_size, MAX_CLASSIC_TX_SIZE_BYTES);

        // Soroban with larger limit
        let config = FlowControlConfig::new(25, Some(500_000), 0, 0);
        assert_eq!(
            config.max_tx_size,
            500_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );
    }

    #[test]
    fn test_flow_control_config_update() {
        let mut config = FlowControlConfig::default();

        // Increase
        let diff = config.update_for_soroban(500_000);
        assert!(diff > 0);
        assert_eq!(
            config.max_tx_size,
            500_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );

        // Decrease: diff is 0 but max_tx_size updates to reflect current config.
        // 10_000 + buffer < classic limit, so max_tx_size = classic limit.
        let diff = config.update_for_soroban(10_000);
        assert_eq!(diff, 0);
        assert_eq!(config.max_tx_size, MAX_CLASSIC_TX_SIZE_BYTES);
    }

    #[test]
    fn test_audit_471_soroban_decrease_updates_max_tx_size() {
        // stellar-core always updates mMaxTxSize to max(classicTxSize, newSorobanMax)
        // even on decreases. Verify henyey matches: after a decrease, max_tx_size
        // should reflect the lower value, not stay at the old higher value.
        let mut config = FlowControlConfig::default();

        // First: increase to 500_000 + buffer
        let diff = config.update_for_soroban(500_000);
        assert!(diff > 0);
        let high_max = config.max_tx_size;
        assert_eq!(high_max, 500_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER);

        // Now: decrease to 200_000 + buffer. diff should be 0 (only increases
        // are propagated to flow control capacity), but max_tx_size should update.
        let diff = config.update_for_soroban(200_000);
        assert_eq!(diff, 0);
        // Per stellar-core: mMaxTxSize = max(classicTxSize, 200_000 + buffer)
        assert_eq!(
            config.max_tx_size,
            200_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );

        // Transactions above the new limit should be rejected
        assert!(!config.is_tx_size_valid(200_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER + 1));
    }

    #[test]
    fn test_is_tx_size_valid() {
        let config = FlowControlConfig::default();
        assert!(config.is_tx_size_valid(50_000));
        assert!(config.is_tx_size_valid(MAX_CLASSIC_TX_SIZE_BYTES));
        assert!(!config.is_tx_size_valid(MAX_CLASSIC_TX_SIZE_BYTES + 1));
    }

    #[test]
    fn test_decrease_then_increase_reports_correct_diff() {
        // After a decrease, a subsequent increase should report diff relative
        // to the decreased value, not the original high value.
        let mut config = FlowControlConfig::default();

        // Increase to 500k + buffer
        config.update_for_soroban(500_000);
        assert_eq!(
            config.max_tx_size,
            500_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );

        // Decrease to 200k + buffer
        let diff = config.update_for_soroban(200_000);
        assert_eq!(diff, 0);
        assert_eq!(
            config.max_tx_size,
            200_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );

        // Re-increase to 300k + buffer: diff should be relative to 200k, not 500k
        let diff = config.update_for_soroban(300_000);
        assert_eq!(
            diff,
            (300_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER)
                - (200_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER)
        );
        assert_eq!(
            config.max_tx_size,
            300_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );
    }

    #[test]
    fn test_decrease_to_same_value_returns_zero_diff() {
        let mut config = FlowControlConfig::default();

        config.update_for_soroban(500_000);
        let diff = config.update_for_soroban(500_000);
        assert_eq!(diff, 0);
        assert_eq!(
            config.max_tx_size,
            500_000 + FLOW_CONTROL_BYTES_EXTRA_BUFFER
        );
    }
}
