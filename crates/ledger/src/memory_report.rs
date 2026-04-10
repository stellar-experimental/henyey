//! Process-level memory reporting and per-component breakdown.
//!
//! This module provides [`MemoryReport`] which captures a complete memory
//! snapshot at a point in time: OS-level RSS, jemalloc allocator stats,
//! and per-component heap estimates.
//!
//! Reports are emitted periodically (every 64 ledgers) via structured
//! tracing fields for machine parsing.

use henyey_common::memory::ComponentMemory;
use henyey_common::LedgerSeq;
use tracing::info;

/// Process-level memory breakdown parsed from `/proc/self/status`.
#[derive(Debug, Clone, Default)]
pub struct ProcessMemory {
    /// Total resident set size in bytes (VmRSS).
    pub rss_bytes: u64,
    /// Anonymous (heap + stack) RSS in bytes (RssAnon).
    pub anon_rss_bytes: u64,
    /// File-backed (mmap) RSS in bytes (RssFile).
    pub file_rss_bytes: u64,
}

impl ProcessMemory {
    /// Capture current process memory from `/proc/self/status`.
    ///
    /// Returns zeroed struct on non-Linux or on error.
    pub fn capture() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::parse_proc_status()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn parse_proc_status() -> Self {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return Self::default();
        };

        let mut result = Self::default();
        for line in status.lines() {
            let (key, value_kb) = match line.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            // Values are in "NNNN kB" format
            let kb: u64 = value_kb
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let bytes = kb * 1024;

            match key {
                "VmRSS" => result.rss_bytes = bytes,
                "RssAnon" => result.anon_rss_bytes = bytes,
                "RssFile" => result.file_rss_bytes = bytes,
                _ => {}
            }
        }
        result
    }
}

/// jemalloc allocator statistics.
///
/// All fields are zero when the `jemalloc` feature is not enabled.
#[derive(Debug, Clone, Default)]
pub struct AllocatorStats {
    /// Bytes requested by the application (malloc'd and not yet freed).
    pub allocated: u64,
    /// Bytes in active pages (superset of allocated).
    pub active: u64,
    /// Bytes resident in physical memory.
    pub resident: u64,
    /// Total bytes mapped by the allocator.
    pub mapped: u64,
    /// Bytes retained (returned to OS but still mapped).
    pub retained: u64,
}

impl AllocatorStats {
    /// Capture current jemalloc stats.
    ///
    /// Returns zeroed struct when the `jemalloc` feature is not enabled.
    pub fn capture() -> Self {
        #[cfg(feature = "jemalloc")]
        {
            Self::read_jemalloc()
        }
        #[cfg(not(feature = "jemalloc"))]
        {
            Self::default()
        }
    }

    #[cfg(feature = "jemalloc")]
    fn read_jemalloc() -> Self {
        use tikv_jemalloc_ctl::{epoch, stats};

        // Advance the epoch to get fresh stats
        let _ = epoch::advance();

        Self {
            allocated: stats::allocated::read().unwrap_or(0) as u64,
            active: stats::active::read().unwrap_or(0) as u64,
            resident: stats::resident::read().unwrap_or(0) as u64,
            mapped: stats::mapped::read().unwrap_or(0) as u64,
            retained: stats::retained::read().unwrap_or(0) as u64,
        }
    }
}

/// Complete memory snapshot for a single point in time.
#[derive(Debug, Clone)]
pub struct MemoryReport {
    pub ledger_seq: LedgerSeq,
    pub process: ProcessMemory,
    pub allocator: AllocatorStats,
    pub components: Vec<ComponentMemory>,
}

impl MemoryReport {
    /// Create a new memory report.
    pub fn new(ledger_seq: LedgerSeq, components: Vec<ComponentMemory>) -> Self {
        Self {
            ledger_seq,
            process: ProcessMemory::capture(),
            allocator: AllocatorStats::capture(),
            components,
        }
    }

    /// Total heap bytes reported by heap-allocated components (excludes mmap).
    pub fn component_total(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| c.is_heap)
            .map(|c| c.heap_bytes)
            .sum()
    }

    /// Total non-heap (mmap/file-backed) bytes.
    pub fn non_heap_total(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| !c.is_heap)
            .map(|c| c.heap_bytes)
            .sum()
    }

    /// Bytes allocated but not accounted for by components.
    ///
    /// Positive values indicate heap usage not yet instrumented.
    /// Negative values indicate over-counting (e.g., shared Arcs counted twice).
    pub fn unaccounted(&self) -> i64 {
        self.allocator.allocated as i64 - self.component_total() as i64
    }

    /// Fragmentation percentage: extra resident memory beyond what the app allocated.
    ///
    /// `(resident - allocated) / allocated * 100`
    pub fn fragmentation_pct(&self) -> f64 {
        if self.allocator.allocated == 0 {
            return 0.0;
        }
        (self.allocator.resident as f64 - self.allocator.allocated as f64)
            / self.allocator.allocated as f64
            * 100.0
    }

    /// Emit structured log lines for the report.
    pub fn log(&self) {
        let to_mb = |b: u64| b as f64 / (1024.0 * 1024.0);

        info!(
            ledger_seq = self.ledger_seq.get(),
            rss_mb = format!("{:.0}", to_mb(self.process.rss_bytes)),
            anon_rss_mb = format!("{:.0}", to_mb(self.process.anon_rss_bytes)),
            file_rss_mb = format!("{:.0}", to_mb(self.process.file_rss_bytes)),
            jemalloc_allocated_mb = format!("{:.0}", to_mb(self.allocator.allocated)),
            jemalloc_resident_mb = format!("{:.0}", to_mb(self.allocator.resident)),
            fragmentation_pct = format!("{:.1}", self.fragmentation_pct()),
            heap_components_mb = format!("{:.0}", to_mb(self.component_total())),
            mmap_mb = format!("{:.0}", to_mb(self.non_heap_total())),
            unaccounted_mb = format!("{:.0}", to_mb(self.unaccounted().unsigned_abs())),
            unaccounted_sign = if self.unaccounted() >= 0 { "+" } else { "-" },
            "Memory report summary"
        );

        for c in &self.components {
            info!(
                ledger_seq = self.ledger_seq.get(),
                component = c.name,
                mb = format!("{:.1}", c.heap_mb()),
                entry_count = c.entry_count,
                kind = if c.is_heap { "heap" } else { "mmap" },
                "Memory report component"
            );
        }
    }
}

/// Log a memory snapshot during startup with a phase label.
///
/// Lighter than a full `MemoryReport` — captures RSS and jemalloc stats
/// without per-component breakdowns. Intended for startup milestones where
/// component data structures may not yet be fully constructed.
pub fn log_startup_memory(phase: &str) {
    let pm = ProcessMemory::capture();
    let alloc = AllocatorStats::capture();
    let to_mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    info!(
        phase,
        rss_mb = format!("{:.0}", to_mb(pm.rss_bytes)),
        jemalloc_allocated_mb = format!("{:.0}", to_mb(alloc.allocated)),
        jemalloc_resident_mb = format!("{:.0}", to_mb(alloc.resident)),
        fragmentation_pct = if alloc.allocated > 0 {
            format!(
                "{:.1}",
                (alloc.resident as f64 - alloc.allocated as f64) / alloc.allocated as f64 * 100.0
            )
        } else {
            "n/a".to_string()
        },
        "Startup memory checkpoint"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_memory_capture() {
        let pm = ProcessMemory::capture();
        // On Linux CI, RSS should be nonzero; on other platforms, zeros are fine
        #[cfg(target_os = "linux")]
        assert!(pm.rss_bytes > 0);
        let _ = pm;
    }

    #[test]
    fn test_allocator_stats_capture() {
        // Without jemalloc feature, all zeros
        let stats = AllocatorStats::capture();
        #[cfg(not(feature = "jemalloc"))]
        {
            assert_eq!(stats.allocated, 0);
            assert_eq!(stats.resident, 0);
        }
        let _ = stats;
    }

    #[test]
    fn test_memory_report_arithmetic() {
        let report = MemoryReport {
            ledger_seq: 100.into(),
            process: ProcessMemory::default(),
            allocator: AllocatorStats {
                allocated: 1000,
                active: 1100,
                resident: 1200,
                mapped: 1500,
                retained: 300,
            },
            components: vec![
                ComponentMemory::new("a", 400, 10),
                ComponentMemory::new("b", 300, 20),
            ],
        };

        assert_eq!(report.component_total(), 700);
        assert_eq!(report.unaccounted(), 300);
        assert!((report.fragmentation_pct() - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_fragmentation_zero_allocated() {
        let report = MemoryReport {
            ledger_seq: 0.into(),
            process: ProcessMemory::default(),
            allocator: AllocatorStats::default(),
            components: vec![],
        };
        assert_eq!(report.fragmentation_pct(), 0.0);
    }
}
