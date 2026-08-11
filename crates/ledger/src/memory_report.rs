//! Process-level memory reporting and per-component breakdown.
//!
//! This module provides [`MemoryReport`] which captures a complete memory
//! snapshot at a point in time: OS-level RSS, jemalloc allocator stats,
//! and per-component heap estimates.
//!
//! Reports are emitted periodically (every 64 ledgers) via structured
//! tracing fields for machine parsing.

use henyey_common::memory::ComponentMemory;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
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

    /// Live small-class bytes across all arenas (`stats.arenas.<all>.small.allocated`).
    pub small_allocated: u64,
    /// Live large-class bytes across all arenas (`stats.arenas.<all>.large.allocated`).
    /// jemalloc 5.x folds the former "huge" class into "large".
    pub large_allocated: u64,

    /// Effective `opt.retain` (false ⇒ freed extents are munmapped, not retained).
    pub opt_retain: bool,
    /// Effective `opt.dirty_decay_ms` (ms before dirty pages are purged; -1 = never).
    pub opt_dirty_decay_ms: i64,
    /// Effective `opt.muzzy_decay_ms` (ms before muzzy pages are decommitted; -1 = never).
    pub opt_muzzy_decay_ms: i64,
    /// Effective `opt.background_thread` (async purge thread enabled).
    pub opt_background_thread: bool,
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

    /// Allocator-side 3-way attribution of `allocated` into small / large / huge.
    ///
    /// `small` and `large` come directly from the merged-arena `stats.arenas`
    /// MIBs; `huge` is the residual `allocated − small − large`, so the three
    /// components sum *exactly* to `allocated`. The split is an allocator-level
    /// view of which size class a steady live-bytes climb concentrates in — it
    /// is **not** a per-subsystem ownership breakdown. A monotonic climb in one
    /// class points at the leaking structure's allocation size, which an
    /// operator soak + a targeted follow-up maps to the exact subsystem (#3237).
    ///
    /// When the per-arena MIBs are unavailable (`small`/`large` both zero, e.g.
    /// jemalloc built without `--enable-stats`), `huge` collapses to the whole
    /// of `allocated` — i.e. a single `arena_total`-style fallback — so the
    /// invariant (sum == allocated) still holds and nothing panics.
    pub fn arena_split(&self) -> (u64, u64, u64) {
        let small = self.small_allocated;
        let large = self.large_allocated;
        // Residual; saturating so synthetic/over-counted inputs can't underflow.
        let huge = self.allocated.saturating_sub(small).saturating_sub(large);
        (small, large, huge)
    }

    #[cfg(feature = "jemalloc")]
    fn read_jemalloc() -> Self {
        use tikv_jemalloc_ctl::{epoch, opt, raw, stats};

        // Advance the epoch to get fresh stats
        let _ = epoch::advance();

        // Merged-arena ("all arenas") small/large live-bytes. jemalloc's
        // `MALLCTL_ARENAS_ALL` sentinel is 4096; reading
        // `stats.arenas.4096.{small,large}.allocated` aggregates every arena.
        // Best-effort: any failure (e.g. stats disabled) leaves the field 0,
        // and `arena_split()` then attributes everything to the `huge`
        // residual (the `arena_total` fallback).
        let small_allocated = unsafe {
            raw::read::<libc_size_t>(b"stats.arenas.4096.small.allocated\0")
        }
        .unwrap_or(0) as u64;
        let large_allocated = unsafe {
            raw::read::<libc_size_t>(b"stats.arenas.4096.large.allocated\0")
        }
        .unwrap_or(0) as u64;

        // Effective allocator config — proves the malloc_conf string was
        // actually parsed (see #3237). `opt.*` is read via the prefixed
        // mallctl, so these reflect what jemalloc is really running.
        let opt_retain = unsafe { raw::read::<bool>(b"opt.retain\0") }.unwrap_or(false);
        let opt_dirty_decay_ms =
            unsafe { raw::read::<isize>(b"opt.dirty_decay_ms\0") }.unwrap_or(0) as i64;
        let opt_muzzy_decay_ms =
            unsafe { raw::read::<isize>(b"opt.muzzy_decay_ms\0") }.unwrap_or(0) as i64;
        let opt_background_thread = opt::background_thread::read().unwrap_or(false);

        Self {
            allocated: stats::allocated::read().unwrap_or(0) as u64,
            active: stats::active::read().unwrap_or(0) as u64,
            resident: stats::resident::read().unwrap_or(0) as u64,
            mapped: stats::mapped::read().unwrap_or(0) as u64,
            retained: stats::retained::read().unwrap_or(0) as u64,
            small_allocated,
            large_allocated,
            opt_retain,
            opt_dirty_decay_ms,
            opt_muzzy_decay_ms,
            opt_background_thread,
        }
    }
}

/// `libc::size_t` without a `libc` dependency: `usize` is byte-identical to
/// jemalloc's `size_t` on every platform we build for, and `raw::read::<T>` is
/// a sized memcpy of `size_of::<T>()` bytes.
#[cfg(feature = "jemalloc")]
#[allow(non_camel_case_types)]
type libc_size_t = usize;

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

        let (small, large, huge) = self.allocator.arena_split();

        info!(
            memory_report = true,
            ledger_seq = self.ledger_seq,
            rss_mb = format!("{:.0}", to_mb(self.process.rss_bytes)),
            anon_rss_mb = format!("{:.0}", to_mb(self.process.anon_rss_bytes)),
            file_rss_mb = format!("{:.0}", to_mb(self.process.file_rss_bytes)),
            jemalloc_allocated_mb = format!("{:.0}", to_mb(self.allocator.allocated)),
            jemalloc_resident_mb = format!("{:.0}", to_mb(self.allocator.resident)),
            jemalloc_retained_mb = format!("{:.0}", to_mb(self.allocator.retained)),
            fragmentation_pct = format!("{:.1}", self.fragmentation_pct()),
            heap_components_mb = format!("{:.0}", to_mb(self.component_total())),
            mmap_mb = format!("{:.0}", to_mb(self.non_heap_total())),
            unaccounted_mb = format!("{:.0}", to_mb(self.unaccounted().unsigned_abs())),
            unaccounted_sign = if self.unaccounted() >= 0 { "+" } else { "-" },
            // Allocator-side size-class split of jemalloc `allocated` — see
            // `AllocatorStats::arena_split`. Localizes which class an
            // unaccounted live-bytes climb concentrates in (#3237).
            arena_small_mb = format!("{:.0}", to_mb(small)),
            arena_large_mb = format!("{:.0}", to_mb(large)),
            arena_huge_mb = format!("{:.0}", to_mb(huge)),
            // Effective allocator config — observable proof malloc_conf was
            // parsed (retain=false, 1000ms decay, background_thread=true).
            opt_retain = self.allocator.opt_retain,
            opt_dirty_decay_ms = self.allocator.opt_dirty_decay_ms,
            opt_muzzy_decay_ms = self.allocator.opt_muzzy_decay_ms,
            opt_background_thread = self.allocator.opt_background_thread,
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

        // Emit the allocator-side size-class split as synthetic components so
        // it shows up alongside the per-subsystem heap components. These are an
        // allocator-level view (NOT per-subsystem ownership) — labelled `arena`
        // and `synthetic_class` so they are never mistaken for owned structures.
        for (name, bytes) in [
            ("arena_small", small),
            ("arena_large", large),
            ("arena_huge", huge),
        ] {
            info!(
                ledger_seq = self.ledger_seq,
                component = name,
                mb = format!("{:.1}", bytes as f64 / (1024.0 * 1024.0)),
                entry_count = 0u64,
                kind = "synthetic_class",
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
        // Surface effective allocator config at startup so a no-op malloc_conf
        // is visible from the first checkpoint (#3237).
        opt_retain = alloc.opt_retain,
        opt_dirty_decay_ms = alloc.opt_dirty_decay_ms,
        opt_muzzy_decay_ms = alloc.opt_muzzy_decay_ms,
        opt_background_thread = alloc.opt_background_thread,
        "Startup memory checkpoint"
    );

    // Route this checkpoint into the startup peak-RSS sampler's finer
    // sub-phase tag (#3239). No-op unless a sampler is registered (steady-state
    // `% 64` reports and unit tests leave it unregistered) and the string maps
    // to a known sub-phase. Observability-only: stores an AtomicU8 phase tag
    // read by the sampler thread; no effect on hashes/timing/behavior.
    crate::peak_rss_sampler::note_checkpoint(phase);
}

/// Emit a cheap single-line memory *sample* every ledger close (#3759).
///
/// The full [`MemoryReport::log`] runs only every 64 ledgers (~5 min) and walks
/// every component, so it is too heavy to run per-close and — sampling at an
/// arbitrary phase relative to a 60 s allocation cycle — aliases short-period
/// RSS swings. This function reuses the same `/proc` + jemalloc captures as the
/// full report (process RSS split, jemalloc allocated/resident, the exact
/// `arena_small`/`large`/`huge` size-class split, and fragmentation) and emits
/// them as **one** structured line at ~5 s cadence — well under the Nyquist
/// bound for a 60 s cycle — so both the trough and peak of the sawtooth land in
/// the log stream.
///
/// The line carries a **distinct** `memory_sample = true` field. It deliberately
/// does **not** emit the reserved `memory_report = true` field (see
/// [`MEMORY_REPORT_FIELD`] and the `test_memory_report_emits_field_*` contract):
/// monitoring consumers grep the two independently. There is no per-component
/// walk — that is what keeps it cheap enough to run on every close.
///
/// **Wall-clock throttle.** Steady-state closes are ~5 s apart, so every close
/// emits a sample. But `LedgerCloseContext::commit` also runs on the catchup
/// replay path (`replay_via_close_ledger`), where many ledgers close per second
/// — a per-close `info!` line there is pure log-volume noise. To avoid that
/// (flagged in PR #3843 review), samples are throttled to at most one per
/// [`MIN_SAMPLE_INTERVAL_MS`]. That interval (1 s) is far under the Nyquist
/// bound for the 60 s RSS sawtooth, so the steady-state sampling this exists for
/// is unaffected, while catchup collapses to ≤1 line/s.
pub fn log_periodic_sample(ledger_seq: u32) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last_ms = LAST_SAMPLE_UNIX_MS.load(Ordering::Relaxed);
    if !should_emit_sample(now_ms, last_ms) {
        return;
    }
    LAST_SAMPLE_UNIX_MS.store(now_ms, Ordering::Relaxed);
    emit_memory_sample(ledger_seq);
}

/// Minimum wall-clock spacing between emitted memory samples (see
/// [`log_periodic_sample`]).
const MIN_SAMPLE_INTERVAL_MS: u64 = 1_000;

/// Wall-clock time (ms since UNIX epoch) the last sample was emitted; `0` = never.
static LAST_SAMPLE_UNIX_MS: AtomicU64 = AtomicU64::new(0);

/// Pure throttle decision, factored out for deterministic testing. Emits when at
/// least [`MIN_SAMPLE_INTERVAL_MS`] has elapsed since the last sample. A `now_ms`
/// of `0` (clock read failed) fails open so a broken clock degrades to per-close
/// emission rather than silence.
fn should_emit_sample(now_ms: u64, last_ms: u64) -> bool {
    now_ms == 0 || now_ms.saturating_sub(last_ms) >= MIN_SAMPLE_INTERVAL_MS
}

/// Emit the actual single-line memory sample (unthrottled). Split out from
/// [`log_periodic_sample`] so the throttle wrapper stays testable in isolation.
fn emit_memory_sample(ledger_seq: u32) {
    let pm = ProcessMemory::capture();
    let alloc = AllocatorStats::capture();
    let (small, large, huge) = alloc.arena_split();
    let to_mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let fragmentation_pct = if alloc.allocated > 0 {
        (alloc.resident as f64 - alloc.allocated as f64) / alloc.allocated as f64 * 100.0
    } else {
        0.0
    };

    info!(
        memory_sample = true,
        ledger_seq = ledger_seq,
        rss_mb = format!("{:.0}", to_mb(pm.rss_bytes)),
        anon_rss_mb = format!("{:.0}", to_mb(pm.anon_rss_bytes)),
        file_rss_mb = format!("{:.0}", to_mb(pm.file_rss_bytes)),
        jemalloc_allocated_mb = format!("{:.0}", to_mb(alloc.allocated)),
        jemalloc_resident_mb = format!("{:.0}", to_mb(alloc.resident)),
        arena_small_mb = format!("{:.0}", to_mb(small)),
        arena_large_mb = format!("{:.0}", to_mb(large)),
        arena_huge_mb = format!("{:.0}", to_mb(huge)),
        fragmentation_pct = format!("{:.1}", fragmentation_pct),
        "Memory sample"
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
    fn test_arena_split_sums_to_allocated() {
        // small + large + huge must always equal `allocated`.
        let stats = AllocatorStats {
            allocated: 1000,
            small_allocated: 600,
            large_allocated: 250,
            ..Default::default()
        };
        let (small, large, huge) = stats.arena_split();
        assert_eq!(small, 600);
        assert_eq!(large, 250);
        assert_eq!(huge, 150, "huge is the residual allocated - small - large");
        assert_eq!(small + large + huge, stats.allocated);
    }

    #[test]
    fn test_arena_split_fallback_when_mibs_unavailable() {
        // When the per-arena MIBs are unavailable, small/large are zero and the
        // whole of `allocated` collapses into the `huge` (arena_total) bucket.
        let stats = AllocatorStats {
            allocated: 4096,
            small_allocated: 0,
            large_allocated: 0,
            ..Default::default()
        };
        let (small, large, huge) = stats.arena_split();
        assert_eq!((small, large), (0, 0));
        assert_eq!(huge, 4096);
        assert_eq!(small + large + huge, stats.allocated);
    }

    #[test]
    fn test_arena_split_saturates_on_overcount() {
        // If small + large somehow exceed `allocated` (e.g. synthetic inputs or
        // a transient epoch skew), `huge` saturates to 0 — never underflows.
        let stats = AllocatorStats {
            allocated: 500,
            small_allocated: 400,
            large_allocated: 300,
            ..Default::default()
        };
        let (small, large, huge) = stats.arena_split();
        assert_eq!(huge, 0, "residual saturates rather than underflowing");
        assert_eq!((small, large), (400, 300));
    }

    #[test]
    fn test_opt_fields_plumb_into_report() {
        // The opt.* config fields round-trip through AllocatorStats and are
        // readable from the report's allocator snapshot.
        let report = MemoryReport {
            ledger_seq: 7,
            process: ProcessMemory::default(),
            allocator: AllocatorStats {
                allocated: 2048,
                opt_retain: false,
                opt_dirty_decay_ms: 1000,
                opt_muzzy_decay_ms: 1000,
                opt_background_thread: true,
                ..Default::default()
            },
            components: vec![],
        };
        assert!(!report.allocator.opt_retain);
        assert_eq!(report.allocator.opt_dirty_decay_ms, 1000);
        assert_eq!(report.allocator.opt_muzzy_decay_ms, 1000);
        assert!(report.allocator.opt_background_thread);
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
                ..Default::default()
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

    /// Issue #3759: `log_periodic_sample` emits exactly one event carrying the
    /// distinct `memory_sample = true` field, and must NOT emit the reserved
    /// `memory_report = true` field (whose exclusivity contract is guarded by
    /// `test_memory_report_emits_field_structured`). This is the cheap per-close
    /// line that resolves the 60 s RSS sawtooth in the log stream.
    #[test]
    fn test_periodic_sample_emits_distinct_field() {
        use tracing::{
            field::{Field, Visit},
            subscriber::with_default,
            Event, Metadata, Subscriber,
        };

        #[derive(Default)]
        struct FieldFlags {
            has_sample: bool,
            has_report: bool,
        }
        impl Visit for FieldFlags {
            fn record_bool(&mut self, field: &Field, value: bool) {
                if value && field.name() == "memory_sample" {
                    self.has_sample = true;
                }
                if value && field.name() == MEMORY_REPORT_FIELD {
                    self.has_report = true;
                }
            }
            fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
        }

        #[derive(Default, Clone)]
        struct SampleFieldSubscriber {
            sample_count: Arc<AtomicUsize>,
            report_count: Arc<AtomicUsize>,
            total_events: Arc<AtomicUsize>,
        }
        impl Subscriber for SampleFieldSubscriber {
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
                let mut flags = FieldFlags::default();
                event.record(&mut flags);
                if flags.has_sample {
                    self.sample_count.fetch_add(1, Ordering::SeqCst);
                }
                if flags.has_report {
                    self.report_count.fetch_add(1, Ordering::SeqCst);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let sub = SampleFieldSubscriber::default();
        let sample_count = sub.sample_count.clone();
        let report_count = sub.report_count.clone();
        let total_events = sub.total_events.clone();

        // Reset the wall-clock throttle so this call is guaranteed to emit,
        // independent of any other sample this test binary may have taken.
        LAST_SAMPLE_UNIX_MS.store(0, Ordering::Relaxed);
        with_default(sub, || {
            log_periodic_sample(4242);
        });

        assert_eq!(
            sample_count.load(Ordering::SeqCst),
            1,
            "log_periodic_sample() must emit exactly one event with memory_sample=true"
        );
        assert_eq!(
            report_count.load(Ordering::SeqCst),
            0,
            "log_periodic_sample() must NOT emit the reserved {MEMORY_REPORT_FIELD} field"
        );
        assert_eq!(
            total_events.load(Ordering::SeqCst),
            1,
            "log_periodic_sample() must emit exactly one (single-line) event"
        );
    }

    /// Issue #3759 / PR #3843: the wall-clock throttle in `log_periodic_sample`
    /// suppresses back-to-back samples closer than `MIN_SAMPLE_INTERVAL_MS`.
    /// This is what keeps catchup replay (many closes/sec) from emitting one
    /// info line per replayed ledger, while leaving steady-state per-close
    /// sampling (closes ~5 s apart) untouched.
    #[test]
    fn test_periodic_sample_throttle_decision() {
        // Never sampled before ⇒ emit.
        assert!(should_emit_sample(1_000_000, 0));
        // Exactly at the interval boundary ⇒ emit.
        assert!(should_emit_sample(10_000 + MIN_SAMPLE_INTERVAL_MS, 10_000));
        // Just under the interval (catchup: many closes/sec) ⇒ suppress.
        assert!(!should_emit_sample(
            10_000 + MIN_SAMPLE_INTERVAL_MS - 1,
            10_000
        ));
        // Same-millisecond close ⇒ suppress.
        assert!(!should_emit_sample(10_000, 10_000));
        // Clock read failed (now_ms == 0) ⇒ fail open (emit).
        assert!(should_emit_sample(0, 10_000));
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
