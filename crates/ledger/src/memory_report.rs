//! Process-level memory reporting and per-component breakdown.
//!
//! This module provides [`MemoryReport`] which captures a complete memory
//! snapshot at a point in time: OS-level RSS, jemalloc allocator stats,
//! and per-component heap estimates.
//!
//! Reports are emitted periodically (every 64 ledgers) via structured
//! tracing fields for machine parsing.

use henyey_common::memory::ComponentMemory;
use tracing::info;

/// Name of the structured tracing field emitted by [`MemoryReport::log`].
///
/// This field is **reserved exclusively** for the memory-report summary
/// event.  No other code path should emit an event with this field name.
///
/// External monitoring tools (e.g. monitor-tick) grep rendered log output
/// for this field to detect memory report presence.  The constant is a
/// documentation anchor — the real mechanical guard is the
/// `test_memory_report_emits_field_*` tests.  **Do not rename this field
/// without updating the tests and all monitoring consumers.**
#[cfg(test)]
pub(crate) const MEMORY_REPORT_FIELD: &str = "memory_report";

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
    pub ledger_seq: u32,
    pub process: ProcessMemory,
    pub allocator: AllocatorStats,
    pub components: Vec<ComponentMemory>,
}

impl MemoryReport {
    /// Create a new memory report.
    pub fn new(ledger_seq: u32, components: Vec<ComponentMemory>) -> Self {
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
            .map(|c| c.bytes)
            .sum()
    }

    /// Total non-heap (mmap/file-backed) bytes.
    pub fn non_heap_total(&self) -> u64 {
        self.components
            .iter()
            .filter(|c| !c.is_heap)
            .map(|c| c.bytes)
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
    ///
    /// The summary event includes a `memory_report = true` structured tracing
    /// field.  This field is **reserved exclusively** for this event — no other
    /// code path should emit it.  External monitoring tools (e.g. monitor-tick)
    /// grep rendered log output for this field to detect memory report presence.
    /// **Do not rename or remove the field without updating all monitoring
    /// consumers and the `test_memory_report_emits_field_*` test suite.**
    pub fn log(&self) {
        let to_mb = |b: u64| b as f64 / (1024.0 * 1024.0);

        info!(
            memory_report = true,
            ledger_seq = self.ledger_seq,
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
                ledger_seq = self.ledger_seq,
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
            ledger_seq: 100,
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
            ledger_seq: 0,
            process: ProcessMemory::default(),
            allocator: AllocatorStats::default(),
            components: vec![],
        };
        assert_eq!(report.fragmentation_pct(), 0.0);
    }
}

/// Tests for [`MEMORY_REPORT_FIELD`] — the monitoring contract.
///
/// These tests guard that `MemoryReport::log()` emits the `memory_report`
/// structured field and that both the Text and JSON `tracing_subscriber::fmt`
/// formatters render it in grep-able form.
#[cfg(test)]
mod memory_report_field_tests {
    use super::*;
    use std::io;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// Build a minimal `MemoryReport` suitable for testing `log()`.
    fn test_report() -> MemoryReport {
        MemoryReport {
            ledger_seq: 42,
            process: ProcessMemory::default(),
            allocator: AllocatorStats::default(),
            components: vec![ComponentMemory::new("test", 100, 5)],
        }
    }

    /// Verify `MemoryReport::log()` emits the structured field
    /// `memory_report = true` on the summary event, and that component
    /// events do NOT carry the field (exclusivity).
    #[test]
    fn test_memory_report_emits_field_structured() {
        use tracing::{
            field::{Field, Visit},
            subscriber::with_default,
            Event, Metadata, Subscriber,
        };

        #[derive(Default)]
        struct CapturedBool {
            value: Option<bool>,
        }
        impl Visit for CapturedBool {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if field.name() == MEMORY_REPORT_FIELD {
                    self.value = Some(value);
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        #[derive(Default, Clone)]
        struct MemReportFieldSubscriber {
            summary_count: Arc<AtomicUsize>,
            component_has_field: Arc<Mutex<bool>>,
            total_events: Arc<AtomicUsize>,
        }
        impl Subscriber for MemReportFieldSubscriber {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &Event<'_>) {
                self.total_events.fetch_add(1, Ordering::SeqCst);
                let mut cap = CapturedBool::default();
                event.record(&mut cap);
                if let Some(true) = cap.value {
                    self.summary_count.fetch_add(1, Ordering::SeqCst);
                }
                // Check if a component event was seen with the field —
                // that would be a contract violation.
                let mut is_component = false;
                struct MsgVisitor<'a>(&'a mut bool);
                impl Visit for MsgVisitor<'_> {
                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        if field.name() == "message" {
                            let msg = format!("{:?}", value);
                            if msg.contains("Memory report component") {
                                *self.0 = true;
                            }
                        }
                    }
                }
                event.record(&mut MsgVisitor(&mut is_component));
                if is_component && cap.value == Some(true) {
                    *self.component_has_field.lock().unwrap() = true;
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let sub = MemReportFieldSubscriber::default();
        let summary_count = sub.summary_count.clone();
        let component_has_field = sub.component_has_field.clone();
        let total_events = sub.total_events.clone();

        with_default(sub, || {
            test_report().log();
        });

        assert_eq!(
            summary_count.load(Ordering::SeqCst),
            1,
            "MemoryReport::log() must emit exactly one event with {MEMORY_REPORT_FIELD}=true"
        );
        assert!(
            !*component_has_field.lock().unwrap(),
            "Component events must NOT carry the {MEMORY_REPORT_FIELD} field"
        );
        // Summary + 1 component = 2 events minimum
        assert!(
            total_events.load(Ordering::SeqCst) >= 2,
            "Expected at least 2 events (summary + components)"
        );
    }

    /// Verify the Text formatter renders the field as `memory_report=true`,
    /// matching the production formatter construction in `logging.rs:334-341`.
    #[test]
    fn test_memory_report_emits_field_text_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production Text formatter construction (logging.rs:334-341).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .with_ansi(false)
            .with_target(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer);

        with_default(subscriber, || {
            test_report().log();
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("memory_report=true"),
            "Text format must render field as 'memory_report=true' for grep. Got: {output}"
        );
    }

    /// Verify the JSON formatter renders the field as `"memory_report":true`,
    /// matching the production formatter construction in `logging.rs:353-357`.
    #[test]
    fn test_memory_report_emits_field_json_format() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();

        // Mirror production JSON formatter construction (logging.rs:353-357).
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .json()
            .with_span_list(true)
            .with_current_span(true);

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer);

        with_default(subscriber, || {
            test_report().log();
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("\"memory_report\":true"),
            "JSON format must render field as '\"memory_report\":true' for grep. Got: {output}"
        );
    }

    /// A `Write` adapter that appends to a shared `Vec<u8>`.
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
