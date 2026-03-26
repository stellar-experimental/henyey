//! Time utilities for rs-stellar-core.
//!
//! This module provides utilities for working with timestamps in the Stellar
//! protocol. Stellar uses a custom epoch (January 1, 2000) for some internal
//! timestamps, while ledger close times use standard Unix timestamps.
//!
//! # Stellar Epoch
//!
//! The Stellar epoch is January 1, 2000, 00:00:00 UTC (Unix timestamp 946684800).
//! Some internal Stellar timestamps are relative to this epoch rather than the
//! Unix epoch (January 1, 1970).
//!
//! # Example
//!
//! ```rust
//! use henyey_common::time::{
//!     unix_to_stellar_time, stellar_to_unix_time, current_timestamp, STELLAR_EPOCH
//! };
//!
//! // Get current Unix timestamp
//! let now = current_timestamp();
//!
//! // Convert to Stellar time
//! let stellar_time = unix_to_stellar_time(now);
//!
//! // Convert back to Unix time
//! let unix_time = stellar_to_unix_time(stellar_time);
//! assert_eq!(now, unix_time);
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Returns the current Unix timestamp in seconds.
///
/// This uses the system clock and returns seconds since January 1, 1970 UTC.
/// If the system clock is before the Unix epoch (which should never happen
/// in practice), returns 0.
///
/// # Example
///
/// ```rust
/// use henyey_common::time::current_timestamp;
///
/// let now = current_timestamp();
/// // Should be sometime after 2024
/// assert!(now > 1704067200);
/// ```
pub fn current_timestamp() -> u64 {
    duration_since_unix_epoch().as_secs()
}

/// Returns the current Unix timestamp in milliseconds.
///
/// This provides higher precision than [`current_timestamp`] for use cases
/// that need sub-second accuracy.
///
/// # Example
///
/// ```rust
/// use henyey_common::time::current_timestamp_ms;
///
/// let now_ms = current_timestamp_ms();
/// // Should be at least 1000x larger than seconds
/// assert!(now_ms > 1704067200000);
/// ```
pub fn current_timestamp_ms() -> u64 {
    duration_since_unix_epoch().as_millis() as u64
}

fn duration_since_unix_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
}

/// Converts a Unix timestamp (seconds since 1970) to a [`SystemTime`].
///
/// This is useful for interfacing with standard library time types.
///
/// # Example
///
/// ```rust
/// use henyey_common::time::timestamp_to_system_time;
///
/// let system_time = timestamp_to_system_time(1704067200);
/// // system_time represents 2024-01-01 00:00:00 UTC
/// ```
pub fn timestamp_to_system_time(timestamp: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(timestamp)
}

/// The Stellar epoch as a Unix timestamp.
///
/// This is January 1, 2000, 00:00:00 UTC, which equals Unix timestamp 946684800.
/// Stellar timestamps are sometimes expressed relative to this epoch.
pub const STELLAR_EPOCH: u64 = 946684800;

/// Converts a Unix timestamp to a Stellar timestamp.
///
/// Stellar timestamps are relative to January 1, 2000 (the Stellar epoch).
/// This function subtracts the Stellar epoch from a Unix timestamp.
///
/// Uses saturating subtraction, so timestamps before the Stellar epoch
/// will return 0.
///
/// # Example
///
/// ```rust
/// use henyey_common::time::{unix_to_stellar_time, STELLAR_EPOCH};
///
/// // The Stellar epoch itself converts to 0
/// assert_eq!(unix_to_stellar_time(STELLAR_EPOCH), 0);
///
/// // One day after the Stellar epoch
/// assert_eq!(unix_to_stellar_time(STELLAR_EPOCH + 86400), 86400);
/// ```
pub fn unix_to_stellar_time(unix_ts: u64) -> u64 {
    unix_ts.saturating_sub(STELLAR_EPOCH)
}

/// Converts a Stellar timestamp to a Unix timestamp.
///
/// This adds the Stellar epoch to convert from Stellar time back to Unix time.
///
/// Uses saturating addition to prevent overflow.
///
/// # Example
///
/// ```rust
/// use henyey_common::time::{stellar_to_unix_time, STELLAR_EPOCH};
///
/// // 0 in Stellar time is the Stellar epoch in Unix time
/// assert_eq!(stellar_to_unix_time(0), STELLAR_EPOCH);
///
/// // Round-trip conversion
/// let unix_time = 1704067200u64; // 2024-01-01
/// let stellar_time = unix_time - STELLAR_EPOCH;
/// assert_eq!(stellar_to_unix_time(stellar_time), unix_time);
/// ```
pub fn stellar_to_unix_time(stellar_ts: u64) -> u64 {
    stellar_ts.saturating_add(STELLAR_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_timestamp() {
        let ts = current_timestamp();
        // Should be after 2024
        assert!(ts > 1704067200);
    }

    #[test]
    fn test_stellar_time_conversion() {
        let unix_ts = 1704067200u64; // 2024-01-01 00:00:00 UTC
        let stellar_ts = unix_to_stellar_time(unix_ts);
        let back = stellar_to_unix_time(stellar_ts);
        assert_eq!(unix_ts, back);
    }
}
