//! Logging setup and progress tracking for rs-stellar-core.
//!
//! This module provides:
//!
//! - **Logging initialization**: Configure the global tracing subscriber with
//!   appropriate log levels, formats, and filters
//! - **Progress tracking**: Utilities for reporting progress during long-running
//!   operations like catchup
//! - **Dynamic log level changes**: Runtime modification of log levels via the `/ll` endpoint
//!
//! # Log Formats
//!
//! Two output formats are supported:
//!
//! - **Text** ([`LogFormat::Text`]): Human-readable format with optional ANSI colors,
//!   suitable for terminal output
//! - **JSON** ([`LogFormat::Json`]): Structured JSON format suitable for log aggregation
//!   systems like Elasticsearch or Loki
//!
//! # Example
//!
//! ```no_run
//! use henyey_app::logging::{LogConfig, init_with_handle};
//!
//! // Initialize with default settings (INFO level, text format)
//! let handle = init_with_handle(&LogConfig::default()).expect("Failed to initialize logging");
//!
//! // Later, dynamically change log levels
//! handle.set_level("DEBUG").ok();
//! handle.set_partition_level("henyey_scp", "TRACE").ok();
//! ```
//!
//! # Progress Tracking
//!
//! For long-running operations, use [`ProgressTracker`] for general progress
//! or [`CatchupProgress`] for catchup-specific progress reporting:
//!
//! ```
//! use henyey_app::logging::ProgressTracker;
//! use std::time::Duration;
//!
//! let tracker = ProgressTracker::with_total("downloading", 100)
//!     .with_report_interval(Duration::from_secs(1));
//!
//! for i in 0..100 {
//!     // ... do work ...
//!     tracker.inc();
//! }
//! tracker.complete();
//! ```

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tracing::Level;
use tracing_subscriber::reload::Handle;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

/// Log output format selection.
///
/// Determines how log messages are formatted for output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable text format with optional ANSI colors.
    ///
    /// Example output:
    /// ```text
    /// 2024-01-15T10:30:00Z INFO henyey_app::run Ledger closed seq=12345
    /// ```
    #[default]
    Text,
    /// Structured JSON format for machine parsing.
    ///
    /// Example output:
    /// ```json
    /// {"timestamp":"2024-01-15T10:30:00Z","level":"INFO","target":"henyey_app::run","message":"Ledger closed","seq":12345}
    /// ```
    Json,
}

/// Logging configuration options.
///
/// Controls log level, output format, and additional metadata inclusion.
/// Use the constructor methods for common configurations:
///
/// - [`LogConfig::default()`] - INFO level, text format, colors enabled
/// - [`LogConfig::verbose()`] - DEBUG level with source locations
/// - [`LogConfig::json()`] - Production-ready JSON format
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Minimum log level to output.
    pub level: Level,
    /// Output format (text or JSON).
    pub format: LogFormat,
    /// Enable ANSI color codes (text format only).
    pub ansi_colors: bool,
    /// Include file/line source locations in output.
    pub with_source_location: bool,
    /// Include thread IDs in output.
    pub with_thread_ids: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            format: LogFormat::Text,
            ansi_colors: std::io::stderr().is_terminal(),
            with_source_location: false,
            with_thread_ids: false,
        }
    }
}

impl LogConfig {
    /// Create a verbose debug configuration.
    pub fn verbose() -> Self {
        Self {
            level: Level::DEBUG,
            format: LogFormat::Text,
            ansi_colors: true,
            with_source_location: true,
            with_thread_ids: true,
        }
    }

    /// Create a JSON logging configuration (for production).
    pub fn json() -> Self {
        Self {
            level: Level::INFO,
            format: LogFormat::Json,
            ansi_colors: false,
            with_source_location: true,
            with_thread_ids: true,
        }
    }

    /// Set the log level from a string.
    pub fn with_level(mut self, level: &str) -> Self {
        self.level = match level.to_lowercase().as_str() {
            "trace" => Level::TRACE,
            "debug" => Level::DEBUG,
            "info" => Level::INFO,
            "warn" | "warning" => Level::WARN,
            "error" => Level::ERROR,
            _ => Level::INFO,
        };
        self
    }
}

/// Known log partitions matching stellar-core partition names.
///
/// These map to Rust module targets for filtering purposes.
pub const LOG_PARTITIONS: &[(&str, &str)] = &[
    ("Fs", "stellar_core"),             // Filesystem operations
    ("SCP", "henyey_scp"),              // SCP consensus
    ("Bucket", "henyey_bucket"),        // Bucket list
    ("Database", "henyey_db"),          // Database operations
    ("History", "henyey_history"),      // History archives
    ("Process", "henyey_app"),          // Application process
    ("Ledger", "henyey_ledger"),        // Ledger operations
    ("Overlay", "henyey_overlay"),      // P2P overlay
    ("Herder", "henyey_herder"),        // Herder consensus coordination
    ("Tx", "henyey_tx"),                // Transaction processing
    ("LoadGen", "henyey_app::loadgen"), // Load generation
    ("Work", "henyey_work"),            // Work scheduler
    ("Invariant", "henyey_invariant"),  // Invariants
    ("Perf", "henyey_app::perf"),       // Performance metrics
];

/// Handle for dynamically changing log levels at runtime.
///
/// This handle wraps a tracing-subscriber reload handle and provides
/// a convenient API for modifying log levels from the `/ll` endpoint.
#[derive(Clone)]
pub struct LogLevelHandle {
    handle: Arc<Handle<EnvFilter, Registry>>,
    /// Current log levels by partition (partition name -> level string)
    levels: Arc<RwLock<HashMap<String, String>>>,
    /// Global log level
    global_level: Arc<RwLock<String>>,
}

impl LogLevelHandle {
    /// Create a new log level handle.
    fn new(handle: Handle<EnvFilter, Registry>, initial_level: &str) -> Self {
        let mut levels = HashMap::new();
        for (partition, _) in LOG_PARTITIONS {
            levels.insert(partition.to_string(), initial_level.to_string());
        }
        Self {
            handle: Arc::new(handle),
            levels: Arc::new(RwLock::new(levels)),
            global_level: Arc::new(RwLock::new(initial_level.to_string())),
        }
    }

    /// Set the global log level.
    pub fn set_level(&self, level: &str) -> anyhow::Result<()> {
        let level = normalize_level(level)?;
        let filter = self.build_filter(&level, None)?;
        self.handle.reload(filter)?;
        *self.global_level.write().unwrap() = level.clone();
        // Update all partition levels to the new global level
        let mut levels = self.levels.write().unwrap();
        for (partition, _) in LOG_PARTITIONS {
            levels.insert(partition.to_string(), level.clone());
        }
        Ok(())
    }

    /// Set the log level for a specific partition.
    pub fn set_partition_level(&self, partition: &str, level: &str) -> anyhow::Result<()> {
        let level = normalize_level(level)?;
        // Validate partition name
        let target = partition_to_target(partition)
            .ok_or_else(|| anyhow::anyhow!("Unknown partition: {}", partition))?;

        // Update the stored level
        {
            let mut levels = self.levels.write().unwrap();
            levels.insert(partition.to_string(), level.clone());
        }

        // Rebuild and apply filter
        let filter = self.build_filter_with_partitions()?;
        self.handle.reload(filter)?;

        tracing::debug!(partition = %partition, target = %target, level = %level, "Updated partition log level");
        Ok(())
    }

    /// Get the current log levels for all partitions.
    pub fn get_levels(&self) -> HashMap<String, String> {
        let levels = self.levels.read().unwrap();
        let mut result = levels.clone();
        result.insert(
            "Global".to_string(),
            self.global_level.read().unwrap().clone(),
        );
        result
    }

    /// Build an EnvFilter from the current level configuration.
    fn build_filter(
        &self,
        global_level: &str,
        partition_override: Option<(&str, &str)>,
    ) -> anyhow::Result<EnvFilter> {
        let mut filter = EnvFilter::new(global_level)
            .add_directive("hyper=warn".parse()?)
            .add_directive("reqwest=warn".parse()?)
            .add_directive("h2=warn".parse()?);

        if let Some((target, level)) = partition_override {
            let directive = format!("{}={}", target, level);
            filter = filter.add_directive(directive.parse()?);
        }

        Ok(filter)
    }

    /// Build an EnvFilter from all stored partition levels.
    fn build_filter_with_partitions(&self) -> anyhow::Result<EnvFilter> {
        let global = self.global_level.read().unwrap().clone();
        let levels = self.levels.read().unwrap();

        let mut filter = EnvFilter::new(&global)
            .add_directive("hyper=warn".parse()?)
            .add_directive("reqwest=warn".parse()?)
            .add_directive("h2=warn".parse()?);

        for (partition, level) in levels.iter() {
            if level != &global {
                if let Some(target) = partition_to_target(partition) {
                    let directive = format!("{}={}", target, level);
                    if let Ok(d) = directive.parse() {
                        filter = filter.add_directive(d);
                    }
                }
            }
        }

        Ok(filter)
    }
}

/// Normalize a log level string to uppercase canonical form.
fn normalize_level(level: &str) -> anyhow::Result<String> {
    match level.to_uppercase().as_str() {
        "TRACE" => Ok("trace".to_string()),
        "DEBUG" => Ok("debug".to_string()),
        "INFO" => Ok("info".to_string()),
        "WARN" | "WARNING" => Ok("warn".to_string()),
        "ERROR" => Ok("error".to_string()),
        _ => Err(anyhow::anyhow!("Invalid log level: {}", level)),
    }
}

/// Map a partition name to its Rust module target.
fn partition_to_target(partition: &str) -> Option<&'static str> {
    LOG_PARTITIONS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(partition))
        .map(|(_, target)| *target)
}

/// Initialize the global logging subscriber.
///
/// This should be called once at application startup.
/// For dynamic log level changes, use [`init_with_handle`] instead.
pub fn init(config: &LogConfig) -> anyhow::Result<()> {
    let _ = init_with_handle(config)?;
    Ok(())
}

/// Initialize the global logging subscriber and return a handle for dynamic level changes.
///
/// This should be called once at application startup. The returned handle can be
/// used to modify log levels at runtime via the `/ll` HTTP endpoint.
pub fn init_with_handle(config: &LogConfig) -> anyhow::Result<LogLevelHandle> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(config.level.as_str())
            .add_directive("hyper=warn".parse().unwrap())
            .add_directive("reqwest=warn".parse().unwrap())
            .add_directive("h2=warn".parse().unwrap())
    });

    let initial_level = config.level.as_str().to_lowercase();

    match config.format {
        LogFormat::Text => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(config.ansi_colors)
                .with_target(true)
                .with_thread_ids(config.with_thread_ids)
                .with_file(config.with_source_location)
                .with_line_number(config.with_source_location);

            let (filter, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);

            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();

            Ok(LogLevelHandle::new(reload_handle, &initial_level))
        }
        LogFormat::Json => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .json()
                .with_span_list(true)
                .with_current_span(true);

            let (filter, reload_handle) = tracing_subscriber::reload::Layer::new(env_filter);

            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();

            Ok(LogLevelHandle::new(reload_handle, &initial_level))
        }
    }
}

/// Progress tracker for long-running operations.
///
/// Provides periodic progress updates with rate estimation and ETA calculation.
/// Updates are rate-limited to avoid log spam while still providing visibility
/// into operation progress.
///
/// # Features
///
/// - Tracks items processed with optional total count
/// - Calculates processing rate (items/second)
/// - Estimates time remaining when total is known
/// - Rate-limits log output to configurable intervals
///
/// # Example
///
/// ```
/// use henyey_app::logging::ProgressTracker;
/// use std::time::Duration;
///
/// let tracker = ProgressTracker::with_total("processing", 1000)
///     .with_report_interval(Duration::from_secs(5));
///
/// for i in 0..1000 {
///     // Process item...
///     tracker.inc();
/// }
/// tracker.complete();
/// ```
#[derive(Debug)]
pub struct ProgressTracker {
    /// Description of the operation.
    name: String,
    /// Total number of items to process (if known).
    total: Option<u64>,
    /// Number of items processed so far.
    processed: AtomicU64,
    /// Start time of the operation.
    start_time: Instant,
    /// Last time progress was reported.
    last_report: std::sync::Mutex<Instant>,
    /// Minimum interval between reports.
    report_interval: Duration,
    /// Whether the operation has completed.
    completed: AtomicBool,
}

impl ProgressTracker {
    /// Create a new progress tracker.
    pub fn new(name: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            name: name.into(),
            total: None,
            processed: AtomicU64::new(0),
            start_time: now,
            last_report: std::sync::Mutex::new(now),
            report_interval: Duration::from_secs(5),
            completed: AtomicBool::new(false),
        }
    }

    /// Create a progress tracker with a known total.
    pub fn with_total(name: impl Into<String>, total: u64) -> Self {
        let mut tracker = Self::new(name);
        tracker.total = Some(total);
        tracker
    }

    /// Set the minimum interval between progress reports.
    pub fn with_report_interval(mut self, interval: Duration) -> Self {
        self.report_interval = interval;
        self
    }

    /// Increment the processed count by one.
    pub fn inc(&self) {
        self.inc_by(1);
    }

    /// Increment the processed count by a given amount.
    pub fn inc_by(&self, n: u64) {
        let processed = self.processed.fetch_add(n, Ordering::Relaxed) + n;
        self.maybe_report(processed);
    }

    /// Set the processed count directly.
    pub fn set(&self, n: u64) {
        self.processed.store(n, Ordering::Relaxed);
        self.maybe_report(n);
    }

    /// Get the current processed count.
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }

    /// Mark the operation as complete.
    pub fn complete(&self) {
        self.completed.store(true, Ordering::Relaxed);
        let elapsed = self.start_time.elapsed();
        let processed = self.processed.load(Ordering::Relaxed);

        if let Some(total) = self.total {
            tracing::info!(
                name = %self.name,
                processed = processed,
                total = total,
                elapsed_secs = elapsed.as_secs_f64(),
                "Operation completed"
            );
        } else {
            tracing::info!(
                name = %self.name,
                processed = processed,
                elapsed_secs = elapsed.as_secs_f64(),
                "Operation completed"
            );
        }
    }

    /// Report progress if enough time has elapsed.
    fn maybe_report(&self, processed: u64) {
        let mut last_report = self.last_report.lock().unwrap();
        let now = Instant::now();

        if now.duration_since(*last_report) >= self.report_interval {
            *last_report = now;
            drop(last_report);

            let elapsed = self.start_time.elapsed();
            let rate = if elapsed.as_secs_f64() > 0.0 {
                processed as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            };

            if let Some(total) = self.total {
                let percent = (processed as f64 / total as f64) * 100.0;
                let eta_secs = if rate > 0.0 {
                    (total - processed) as f64 / rate
                } else {
                    0.0
                };

                tracing::info!(
                    name = %self.name,
                    processed = processed,
                    total = total,
                    percent = format!("{:.1}%", percent),
                    rate = format!("{:.1}/s", rate),
                    eta_secs = format!("{:.0}s", eta_secs),
                    "Progress"
                );
            } else {
                tracing::info!(
                    name = %self.name,
                    processed = processed,
                    rate = format!("{:.1}/s", rate),
                    "Progress"
                );
            }
        }
    }
}

/// Specialized progress tracker for catchup operations.
///
/// Tracks progress through the various phases of catchup (downloading,
/// applying, replaying) with phase-specific metrics and logging.
#[derive(Debug)]
pub struct CatchupProgress {
    /// Current phase of catchup.
    phase: std::sync::Mutex<CatchupPhase>,
    /// Ledgers downloaded.
    ledgers_downloaded: AtomicU32,
    /// Ledgers applied.
    ledgers_applied: AtomicU32,
    /// Buckets downloaded.
    buckets_downloaded: AtomicU32,
    /// Total buckets to download.
    total_buckets: AtomicU32,
    /// Target ledger sequence.
    target_ledger: AtomicU32,
    /// Start time.
    start_time: Instant,
}

/// Phase of the catchup operation.
///
/// Represents the current stage of the catchup process. Phases progress
/// in order from `Initializing` through to `Complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchupPhase {
    /// Setting up catchup parameters and targets.
    Initializing,
    /// Fetching history archive state (HAS) file.
    DownloadingState,
    /// Downloading bucket files from archives.
    DownloadingBuckets,
    /// Applying downloaded buckets to rebuild ledger state.
    ApplyingBuckets,
    /// Downloading ledger headers and transaction sets.
    DownloadingLedgers,
    /// Replaying transactions to advance to target ledger.
    ReplayingLedgers,
    /// Verifying final state against expected hashes.
    Verifying,
    /// Catchup completed successfully.
    Complete,
}

impl std::fmt::Display for CatchupPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatchupPhase::Initializing => write!(f, "Initializing"),
            CatchupPhase::DownloadingState => write!(f, "Downloading state"),
            CatchupPhase::DownloadingBuckets => write!(f, "Downloading buckets"),
            CatchupPhase::ApplyingBuckets => write!(f, "Applying buckets"),
            CatchupPhase::DownloadingLedgers => write!(f, "Downloading ledgers"),
            CatchupPhase::ReplayingLedgers => write!(f, "Replaying ledgers"),
            CatchupPhase::Verifying => write!(f, "Verifying"),
            CatchupPhase::Complete => write!(f, "Complete"),
        }
    }
}

impl CatchupProgress {
    /// Create a new catchup progress tracker.
    pub fn new() -> Self {
        Self {
            phase: std::sync::Mutex::new(CatchupPhase::Initializing),
            ledgers_downloaded: AtomicU32::new(0),
            ledgers_applied: AtomicU32::new(0),
            buckets_downloaded: AtomicU32::new(0),
            total_buckets: AtomicU32::new(0),
            target_ledger: AtomicU32::new(0),
            start_time: Instant::now(),
        }
    }

    /// Set the target ledger.
    pub fn set_target(&self, ledger: u32) {
        self.target_ledger.store(ledger, Ordering::Relaxed);
    }

    /// Set the total number of buckets to download.
    pub fn set_total_buckets(&self, count: u32) {
        self.total_buckets.store(count, Ordering::Relaxed);
    }

    /// Set the current phase.
    pub fn set_phase(&self, phase: CatchupPhase) {
        let mut current = self.phase.lock().unwrap();
        if *current != phase {
            tracing::info!(
                phase = %phase,
                target_ledger = self.target_ledger.load(Ordering::Relaxed),
                "Catchup phase changed"
            );
            *current = phase;
        }
    }

    /// Get the current phase.
    pub fn phase(&self) -> CatchupPhase {
        *self.phase.lock().unwrap()
    }

    /// Record a ledger download.
    pub fn ledger_downloaded(&self) {
        self.ledgers_downloaded.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a ledger applied.
    pub fn ledger_applied(&self) {
        let applied = self.ledgers_applied.fetch_add(1, Ordering::Relaxed) + 1;
        if applied % 100 == 0 {
            let target = self.target_ledger.load(Ordering::Relaxed);
            tracing::info!(applied = applied, target = target, "Applied ledgers");
        }
    }

    /// Record a bucket download.
    pub fn bucket_downloaded(&self) {
        let downloaded = self.buckets_downloaded.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total_buckets.load(Ordering::Relaxed);
        if total > 0 && (downloaded % 10 == 0 || downloaded == total) {
            tracing::info!(
                downloaded = downloaded,
                total = total,
                percent = format!("{:.1}%", downloaded as f64 / total as f64 * 100.0),
                "Downloaded buckets"
            );
        }
    }

    /// Print a summary of the catchup.
    pub fn summary(&self) {
        let elapsed = self.start_time.elapsed();
        let applied = self.ledgers_applied.load(Ordering::Relaxed);
        let buckets = self.buckets_downloaded.load(Ordering::Relaxed);
        let target = self.target_ledger.load(Ordering::Relaxed);

        tracing::info!(
            target_ledger = target,
            ledgers_applied = applied,
            buckets_downloaded = buckets,
            elapsed_secs = elapsed.as_secs_f64(),
            "Catchup summary"
        );
    }
}

impl Default for CatchupProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Create an Arc-wrapped catchup progress tracker.
pub fn catchup_progress() -> Arc<CatchupProgress> {
    Arc::new(CatchupProgress::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_tracker() {
        let tracker =
            ProgressTracker::with_total("test", 100).with_report_interval(Duration::from_millis(1));

        assert_eq!(tracker.processed(), 0);

        tracker.inc();
        assert_eq!(tracker.processed(), 1);

        tracker.inc_by(10);
        assert_eq!(tracker.processed(), 11);

        tracker.set(50);
        assert_eq!(tracker.processed(), 50);
    }

    #[test]
    fn test_catchup_phase_display() {
        assert_eq!(format!("{}", CatchupPhase::Initializing), "Initializing");
        assert_eq!(
            format!("{}", CatchupPhase::DownloadingBuckets),
            "Downloading buckets"
        );
    }

    #[test]
    fn test_normalize_level() {
        assert_eq!(normalize_level("trace").unwrap(), "trace");
        assert_eq!(normalize_level("TRACE").unwrap(), "trace");
        assert_eq!(normalize_level("Debug").unwrap(), "debug");
        assert_eq!(normalize_level("INFO").unwrap(), "info");
        assert_eq!(normalize_level("warn").unwrap(), "warn");
        assert_eq!(normalize_level("WARNING").unwrap(), "warn");
        assert_eq!(normalize_level("error").unwrap(), "error");
        assert!(normalize_level("invalid").is_err());
    }

    #[test]
    fn test_partition_to_target() {
        assert_eq!(partition_to_target("SCP"), Some("henyey_scp"));
        assert_eq!(partition_to_target("scp"), Some("henyey_scp"));
        assert_eq!(partition_to_target("Bucket"), Some("henyey_bucket"));
        assert_eq!(partition_to_target("Overlay"), Some("henyey_overlay"));
        assert_eq!(partition_to_target("Unknown"), None);
    }

    #[test]
    fn test_log_partitions_complete() {
        // Verify all expected partitions are defined
        let expected = [
            "Fs",
            "SCP",
            "Bucket",
            "Database",
            "History",
            "Process",
            "Ledger",
            "Overlay",
            "Herder",
            "Tx",
            "LoadGen",
            "Work",
            "Invariant",
            "Perf",
        ];
        for partition in expected {
            assert!(
                partition_to_target(partition).is_some(),
                "Missing partition: {}",
                partition
            );
        }
    }
}
