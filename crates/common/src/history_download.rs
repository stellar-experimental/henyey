//! Shared tuning constants for history-archive downloads.
//!
//! These values govern the concurrency and progress-logging cadence of
//! history-archive download work (bucket downloads, checkpoint fetches, and
//! catchup downloads). They live here as a single source of truth so a future
//! upstream change only requires one edit; previously the same values were
//! duplicated across `henyey-historywork`, `henyey-history`, and the `henyey`
//! binary.

/// Maximum number of concurrent download requests, mirroring stellar-core's
/// `MAX_CONCURRENT_SUBPROCESSES` limit.
pub const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Log download progress every N items (and always on the last item).
pub const PROGRESS_REPORT_INTERVAL: u32 = 5;
