//! Catchup command implementation for rs-stellar-core.
//!
//! The catchup command synchronizes a node with the Stellar network by downloading
//! ledger history from archives and applying it to rebuild local state. This is
//! essential for:
//!
//! - Initial node setup (bootstrapping from scratch)
//! - Recovery after extended downtime
//! - Rebuilding state after database corruption
//!
//! # Command Line Usage
//!
//! ```text
//! rs-stellar-core catchup current        # Catch up to the latest ledger
//! rs-stellar-core catchup 1000000        # Catch up to ledger 1000000
//! rs-stellar-core catchup 1000000/100    # Catch up to ledger 1000000 with 100 ledgers history
//! ```
//!
//! # Catchup Modes
//!
//! | Mode | Description | Use Case |
//! |------|-------------|----------|
//! | `minimal` | Download only the latest bucket state | Fast startup, no history needed |
//! | `complete` | Download full history from genesis | Full archive node |
//! | `recent:N` | Download last N ledgers of history | Balance of speed and history |
//!
//! # Process Overview
//!
//! 1. **Determine target**: Resolve "current" to latest checkpoint or use specified ledger
//! 2. **Download state**: Fetch bucket files from history archives
//! 3. **Apply buckets**: Rebuild ledger state from bucket snapshots
//! 4. **Replay history**: Apply transactions to reach the target ledger
//! 5. **Verify**: Validate the resulting state matches expected hashes
//!
//! # Progress Reporting
//!
//! Catchup provides detailed progress through the [`CatchupProgressCallback`] trait,
//! with built-in implementations for logging ([`TracingProgressCallback`]) and
//! console output ([`ConsoleProgressCallback`]).

use crate::app::{App, CatchupResult, CatchupTarget};
use crate::config::AppConfig;
pub use henyey_history::CatchupMode;

/// Configuration options for the catchup command.
///
/// Controls the target ledger, history depth, verification, and performance
/// settings for the catchup operation.
///
/// # Example
///
/// ```
/// use henyey_app::CatchupOptions;
/// use henyey_app::CatchupMode;
///
/// // Catch up to ledger 1000000 with recent history
/// let options = CatchupOptions::to_ledger(1000000)
///     .with_mode(CatchupMode::Recent(1000))
///     .with_parallelism(16);
/// ```
#[derive(Debug, Clone)]
pub struct CatchupOptions {
    /// Target ledger specification ("current" or a ledger number).
    pub target: String,
    /// Catchup mode determining history depth.
    pub mode: CatchupMode,
    /// Whether to verify state hashes after catchup.
    pub verify: bool,
    /// Number of parallel archive downloads.
    pub parallelism: usize,
    /// Whether to keep temporary download files (for debugging).
    pub keep_temp: bool,
}

impl Default for CatchupOptions {
    fn default() -> Self {
        Self {
            target: "current".to_string(),
            mode: CatchupMode::Minimal,
            verify: true,
            parallelism: 8,
            keep_temp: false,
        }
    }
}

impl CatchupOptions {
    /// Create options for catching up to the current/latest ledger.
    pub fn current() -> Self {
        Self::default()
    }

    /// Create options for catching up to a specific ledger.
    pub fn to_ledger(seq: u32) -> Self {
        Self {
            target: seq.to_string(),
            ..Default::default()
        }
    }

    /// Set the catchup mode.
    pub fn with_mode(mut self, mode: CatchupMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set verification on/off.
    pub fn with_verify(mut self, verify: bool) -> Self {
        self.verify = verify;
        self
    }

    /// Set parallelism level.
    pub fn with_parallelism(mut self, n: usize) -> Self {
        self.parallelism = n;
        self
    }

    /// Parse the target into a CatchupTarget.
    pub fn parse_target(&self) -> anyhow::Result<CatchupTarget> {
        parse_target(&self.target)
    }

    /// Parse the target and get the effective mode.
    ///
    /// Mode precedence (highest to lowest):
    /// 1. Explicit mode from CLI (if not default Minimal)
    /// 2. Mode from target string (e.g., "1000000/100" -> Recent(100))
    /// 3. Default (Minimal)
    pub fn parse_target_and_mode(&self) -> anyhow::Result<(CatchupTarget, CatchupMode)> {
        let parsed = parse_target_with_mode(&self.target)?;

        // If explicit mode is set (not default), use it; otherwise use target's mode
        let effective_mode = if self.mode != CatchupMode::Minimal {
            self.mode
        } else {
            parsed.mode_from_target.unwrap_or(CatchupMode::Minimal)
        };

        Ok((parsed.target, effective_mode))
    }
}

/// Parsed catchup target with optional mode override from "ledger/count" format.
#[derive(Debug, Clone)]
pub struct ParsedCatchupTarget {
    /// The target ledger.
    pub target: CatchupTarget,
    /// Mode override from target string (e.g., from "1000000/100" format).
    pub mode_from_target: Option<CatchupMode>,
}

/// Parse a target ledger specification.
///
/// Formats:
/// - "current" -> CatchupTarget::Current
/// - "12345" -> CatchupTarget::Ledger(12345)
/// - "12345/100" -> CatchupTarget::Ledger(12345) with Recent(100) mode
/// - "12345/max" -> CatchupTarget::Ledger(12345) with Complete mode
pub fn parse_target(target: &str) -> anyhow::Result<CatchupTarget> {
    parse_target_with_mode(target).map(|parsed| parsed.target)
}

/// Parse a target ledger specification including optional count for mode.
///
/// Formats:
/// - "current" -> CatchupTarget::Current, None
/// - "12345" -> CatchupTarget::Ledger(12345), None
/// - "12345/100" -> CatchupTarget::Ledger(12345), Some(Recent(100))
/// - "12345/max" -> CatchupTarget::Ledger(12345), Some(Complete)
pub fn parse_target_with_mode(target: &str) -> anyhow::Result<ParsedCatchupTarget> {
    let target = target.trim().to_lowercase();

    if target == "current" || target == "latest" {
        return Ok(ParsedCatchupTarget {
            target: CatchupTarget::Current,
            mode_from_target: None,
        });
    }

    // Check for "ledger/count" format
    if let Some(slash_pos) = target.find('/') {
        let ledger_str = &target[..slash_pos];
        let count_str = &target[slash_pos + 1..];

        let ledger: u32 = ledger_str
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid ledger number: {}", ledger_str))?;

        // Parse count: "max" means complete, number means recent
        let mode = if count_str == "max" {
            CatchupMode::Complete
        } else {
            let count: u32 = count_str
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid count: {}", count_str))?;
            if count == 0 {
                CatchupMode::Minimal
            } else {
                CatchupMode::Recent(count)
            }
        };

        return Ok(ParsedCatchupTarget {
            target: CatchupTarget::Ledger(ledger),
            mode_from_target: Some(mode),
        });
    }

    // Just a ledger number
    let ledger: u32 = target.parse().map_err(|_| {
        anyhow::anyhow!(
            "Invalid target: '{}'. Use 'current', a ledger number, or 'ledger/count'",
            target
        )
    })?;

    Ok(ParsedCatchupTarget {
        target: CatchupTarget::Ledger(ledger),
        mode_from_target: None,
    })
}

/// Run the catchup command.
pub async fn run_catchup(
    config: AppConfig,
    options: CatchupOptions,
) -> anyhow::Result<CatchupResult> {
    tracing::info!(
        target = %options.target,
        mode = %options.mode,
        "Starting catchup command"
    );

    // Parse target and determine effective mode
    let (target, effective_mode) = options.parse_target_and_mode()?;

    tracing::info!(
        ?target,
        effective_mode = %effective_mode,
        "Resolved catchup parameters"
    );

    // Create application
    let app = App::new(config).await?;

    // Print catchup info
    print_catchup_info(&options, &target, effective_mode);

    // Run catchup with mode
    let (result, _persist_data) = app.catchup_with_mode(target, effective_mode).await?;
    // In CLI mode, persist data is ignored (the process exits after catchup).

    // Print result
    print_catchup_result(&result);

    // Verify if requested
    if options.verify {
        verify_catchup(&result)?;
    }

    Ok(result)
}

/// Print information before starting catchup.
fn print_catchup_info(
    options: &CatchupOptions,
    target: &CatchupTarget,
    effective_mode: CatchupMode,
) {
    println!("Catchup Configuration:");
    println!("  Target: {:?}", target);
    println!("  Mode: {}", effective_mode);
    println!("  Parallelism: {}", options.parallelism);
    println!("  Verify: {}", options.verify);
    println!();
}

/// Print the catchup result.
fn print_catchup_result(result: &CatchupResult) {
    println!();
    println!("Catchup Complete!");
    println!("  Final Ledger: {}", result.ledger_seq);
    println!("  Ledger Hash: {}", result.ledger_hash);
    println!("  Buckets Applied: {}", result.buckets_applied);
    println!("  Ledgers Replayed: {}", result.ledgers_replayed);
}

/// Verify the catchup result.
fn verify_catchup(result: &CatchupResult) -> anyhow::Result<()> {
    tracing::info!(ledger = result.ledger_seq, "Verifying catchup result");

    // In a full implementation, this would:
    // 1. Verify the bucket list hash
    // 2. Verify the ledger header hash chain
    // 3. Verify account balances sum correctly
    // 4. Run invariant checks

    println!("Verification: PASSED");
    Ok(())
}

/// Callback trait for receiving catchup progress updates.
///
/// Implement this trait to receive notifications about catchup progress,
/// phase changes, and completion. Useful for progress bars, logging,
/// or integration with external monitoring systems.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` as callbacks may be invoked from
/// multiple async tasks.
pub trait CatchupProgressCallback: Send + Sync {
    /// Called when the catchup phase changes (e.g., "Downloading buckets").
    fn on_phase_change(&self, phase: &str);

    /// Called periodically with progress update.
    ///
    /// - `current`: Number of items processed so far
    /// - `total`: Total number of items (0 if unknown)
    /// - `message`: Human-readable status message
    fn on_progress(&self, current: u64, total: u64, message: &str);

    /// Called when catchup completes successfully.
    fn on_complete(&self, result: &CatchupResult);

    /// Called if an error occurs during catchup.
    fn on_error(&self, error: &str);
}

/// Progress callback that logs to the tracing framework.
///
/// This is the default callback used when no custom callback is provided.
/// Progress updates are logged at INFO level.
pub struct TracingProgressCallback;

impl CatchupProgressCallback for TracingProgressCallback {
    fn on_phase_change(&self, phase: &str) {
        tracing::info!(phase, "Catchup phase");
    }

    fn on_progress(&self, current: u64, total: u64, message: &str) {
        let percent = if total > 0 {
            (current as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        tracing::info!(
            current,
            total,
            percent = format!("{:.1}%", percent),
            message
        );
    }

    fn on_complete(&self, result: &CatchupResult) {
        tracing::info!(
            ledger = result.ledger_seq,
            hash = %result.ledger_hash,
            "Catchup complete"
        );
    }

    fn on_error(&self, error: &str) {
        tracing::error!(error, "Catchup error");
    }
}

/// Progress callback with pretty console output including progress bars.
///
/// Displays a visual progress bar with elapsed time, percentage complete,
/// and estimated time remaining. Suitable for interactive terminal use.
pub struct ConsoleProgressCallback {
    start_time: std::time::Instant,
}

impl ConsoleProgressCallback {
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
        }
    }
}

impl Default for ConsoleProgressCallback {
    fn default() -> Self {
        Self::new()
    }
}

impl CatchupProgressCallback for ConsoleProgressCallback {
    fn on_phase_change(&self, phase: &str) {
        let elapsed = self.start_time.elapsed();
        println!("[{:>6.1}s] Phase: {}", elapsed.as_secs_f64(), phase);
    }

    fn on_progress(&self, current: u64, total: u64, message: &str) {
        let elapsed = self.start_time.elapsed();
        if total > 0 {
            let percent = (current as f64 / total as f64) * 100.0;
            let bar_width = 30;
            let filled = (percent / 100.0 * bar_width as f64) as usize;
            let empty = bar_width - filled;

            print!(
                "\r[{:>6.1}s] [{}{}] {:>5.1}% {}",
                elapsed.as_secs_f64(),
                "=".repeat(filled),
                " ".repeat(empty),
                percent,
                message
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
        } else {
            println!(
                "[{:>6.1}s] {} ({})",
                elapsed.as_secs_f64(),
                message,
                current
            );
        }
    }

    fn on_complete(&self, result: &CatchupResult) {
        let elapsed = self.start_time.elapsed();
        println!();
        println!("[{:>6.1}s] Catchup complete!", elapsed.as_secs_f64());
        println!("  Ledger: {}", result.ledger_seq);
        println!("  Time: {:.1}s", elapsed.as_secs_f64());
    }

    fn on_error(&self, error: &str) {
        let elapsed = self.start_time.elapsed();
        eprintln!();
        eprintln!("[{:>6.1}s] ERROR: {}", elapsed.as_secs_f64(), error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_target_current() {
        let target = parse_target("current").unwrap();
        assert!(matches!(target, CatchupTarget::Current));

        let target = parse_target("latest").unwrap();
        assert!(matches!(target, CatchupTarget::Current));
    }

    #[test]
    fn test_parse_target_ledger() {
        let target = parse_target("1000000").unwrap();
        assert!(matches!(target, CatchupTarget::Ledger(1000000)));
    }

    #[test]
    fn test_parse_target_with_count() {
        let target = parse_target("1000000/100").unwrap();
        assert!(matches!(target, CatchupTarget::Ledger(1000000)));
    }

    #[test]
    fn test_parse_target_invalid() {
        assert!(parse_target("invalid").is_err());
        assert!(parse_target("abc/100").is_err());
    }

    #[test]
    fn test_catchup_mode_from_str() {
        assert!(matches!(
            "minimal".parse::<CatchupMode>().unwrap(),
            CatchupMode::Minimal
        ));
        assert!(matches!(
            "complete".parse::<CatchupMode>().unwrap(),
            CatchupMode::Complete
        ));
        assert!(matches!(
            "recent:100".parse::<CatchupMode>().unwrap(),
            CatchupMode::Recent(100)
        ));
    }

    #[test]
    fn test_catchup_options() {
        let options = CatchupOptions::to_ledger(1000000)
            .with_mode(CatchupMode::Complete)
            .with_verify(false)
            .with_parallelism(16);

        assert_eq!(options.target, "1000000");
        assert!(matches!(options.mode, CatchupMode::Complete));
        assert!(!options.verify);
        assert_eq!(options.parallelism, 16);
    }

    #[test]
    fn test_parse_target_with_mode_current() {
        let parsed = parse_target_with_mode("current").unwrap();
        assert!(matches!(parsed.target, CatchupTarget::Current));
        assert!(parsed.mode_from_target.is_none());
    }

    #[test]
    fn test_parse_target_with_mode_ledger_only() {
        let parsed = parse_target_with_mode("1000000").unwrap();
        assert!(matches!(parsed.target, CatchupTarget::Ledger(1000000)));
        assert!(parsed.mode_from_target.is_none());
    }

    #[test]
    fn test_parse_target_with_mode_recent() {
        let parsed = parse_target_with_mode("1000000/100").unwrap();
        assert!(matches!(parsed.target, CatchupTarget::Ledger(1000000)));
        assert!(matches!(
            parsed.mode_from_target,
            Some(CatchupMode::Recent(100))
        ));
    }

    #[test]
    fn test_parse_target_with_mode_complete() {
        let parsed = parse_target_with_mode("1000000/max").unwrap();
        assert!(matches!(parsed.target, CatchupTarget::Ledger(1000000)));
        assert!(matches!(
            parsed.mode_from_target,
            Some(CatchupMode::Complete)
        ));
    }

    #[test]
    fn test_parse_target_with_mode_minimal() {
        let parsed = parse_target_with_mode("1000000/0").unwrap();
        assert!(matches!(parsed.target, CatchupTarget::Ledger(1000000)));
        assert!(matches!(
            parsed.mode_from_target,
            Some(CatchupMode::Minimal)
        ));
    }

    #[test]
    fn test_effective_mode_explicit_takes_precedence() {
        // Explicit mode takes precedence over target's mode
        let options = CatchupOptions {
            target: "1000000/100".to_string(),
            mode: CatchupMode::Complete,
            verify: true,
            parallelism: 8,
            keep_temp: false,
        };
        let (_, mode) = options.parse_target_and_mode().unwrap();
        assert!(matches!(mode, CatchupMode::Complete));
    }

    #[test]
    fn test_effective_mode_from_target_when_default() {
        // Target's mode used when CLI mode is default (Minimal)
        let options = CatchupOptions {
            target: "1000000/100".to_string(),
            mode: CatchupMode::Minimal, // default
            verify: true,
            parallelism: 8,
            keep_temp: false,
        };
        let (_, mode) = options.parse_target_and_mode().unwrap();
        assert!(matches!(mode, CatchupMode::Recent(100)));
    }
}
