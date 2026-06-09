//! Startup/catchup peak anonymous-RSS sampler.
//!
//! The periodic memory report ([`crate::memory_report::MemoryReport`]) only
//! emits from the ledger-close path, gated on `ledger_seq % 64 == 0`
//! (`crates/ledger/src/manager.rs`). That hook runs *after* the node is
//! already replaying ledgers — by which point the startup/catchup restore peak
//! (HAS download + parallel bucket apply + cache scan) has already been
//! released. As a result the binding memory peak during startup is invisible:
//! a memory-constrained host learns it exceeded the RAM ceiling only by getting
//! OOM-killed.
//!
//! [`StartupPeakRssSampler`] closes that gap. It runs a single background
//! `std::thread` (tokio-independent, like the heartbeat pattern) that polls the
//! process's anonymous RSS roughly once per second, tracking an atomic running
//! maximum and the phase that was current when each new maximum was recorded.
//! When [`StartupPeakRssSampler::stop`] is called (once, after catchup returns
//! and before the event loop is spawned) it joins the thread, emits a greppable
//! `startup_peak_anon_rss_mb=<N> phase=<peak-phase>` one-liner, and returns the
//! peak in bytes so the caller can publish it as a Prometheus gauge.
//!
//! # Observability-only contract
//!
//! The sampler reads `/proc/self/status` (via the existing
//! [`crate::memory_report::ProcessMemory::capture`] parser) and atomics ONLY.
//! It shares no mutable state with the ledger-close / consensus path, performs
//! no allocation on the hot path, and has zero effect on hashes, timing, or any
//! observable protocol behavior. Off-Linux (or on a `/proc` read error) the
//! value-source degrades to 0 — the sampler records peak 0 and never panics,
//! inheriting `ProcessMemory::capture()`'s degrade-to-zero contract.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::info;

use crate::memory_report::ProcessMemory;

/// Default poll interval for the background sampler thread.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Startup phase attributed to a sampled peak.
///
/// The coarse `Startup`/`Catchup` split is the must-have signal (settable from
/// the `run_main_loop` orchestrator alone). The finer `BucketApply`/`CacheScan`
/// tags are nice-to-have refinements set at the existing `log_startup_memory`
/// checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// Generic startup / state-restore window (before catchup is entered).
    Startup = 0,
    /// Parallel bucket-list restore (`restore_from_has_parallel`).
    BucketApply = 1,
    /// In-memory cache scan + pending merges (`scan_level_pairs_for_caches`).
    CacheScan = 2,
    /// History catchup (HAS download + apply).
    Catchup = 3,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::BucketApply,
            2 => Phase::CacheScan,
            3 => Phase::Catchup,
            _ => Phase::Startup,
        }
    }

    /// Stable, greppable string tag for the phase (used in the summary line).
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Startup => "startup",
            Phase::BucketApply => "bucket-apply",
            Phase::CacheScan => "cache-scan",
            Phase::Catchup => "catchup",
        }
    }
}

/// Shared, lock-free state between the sampler thread and the controlling
/// threads. Only the sampler thread writes `peak_bytes` / `peak_phase`;
/// `current_phase` is written by [`StartupPeakRssSampler::set_phase`] from
/// arbitrary threads and read by the sampler thread.
#[derive(Debug)]
struct SamplerState {
    /// Running maximum anonymous RSS in bytes.
    peak_bytes: AtomicU64,
    /// Phase index that was current when `peak_bytes` was last raised.
    peak_phase: AtomicU8,
    /// Phase index currently in effect (written by `set_phase`).
    current_phase: AtomicU8,
    /// Shutdown flag — the thread breaks its loop once this is set.
    stopping: AtomicBool,
}

impl SamplerState {
    fn new() -> Self {
        SamplerState {
            peak_bytes: AtomicU64::new(0),
            peak_phase: AtomicU8::new(Phase::Startup as u8),
            current_phase: AtomicU8::new(Phase::Startup as u8),
            stopping: AtomicBool::new(false),
        }
    }
}

/// A background peak-anon-RSS sampler scoped to the startup/catchup window.
///
/// Construct + start with [`StartupPeakRssSampler::start`] (default `/proc`
/// value-source) or [`StartupPeakRssSampler::start_with_source`] (injected
/// value-source, for tests). Tag phases with [`StartupPeakRssSampler::set_phase`].
/// Finish with [`StartupPeakRssSampler::stop`], which joins the thread, logs the
/// one-liner, and returns the peak in bytes. `stop` is idempotent; a `Drop`
/// fallback guarantees the thread is joined even if `stop` is never called, so
/// the sampler can never outlive the startup window.
pub struct StartupPeakRssSampler {
    state: Arc<SamplerState>,
    handle: Option<JoinHandle<()>>,
    /// Whether the summary one-liner has already been emitted (idempotency).
    stopped: bool,
}

impl StartupPeakRssSampler {
    /// Build a sampler WITHOUT spawning a background thread. Tests drive
    /// [`StartupPeakRssSampler::sample_once`] directly, so the peak/phase logic
    /// is exercised deterministically with no sleeps and no thread races.
    #[cfg(test)]
    fn for_test() -> Self {
        StartupPeakRssSampler {
            state: Arc::new(SamplerState::new()),
            handle: None,
            stopped: false,
        }
    }

    /// Start a sampler backed by the real `/proc/self/status` parser
    /// ([`ProcessMemory::capture`]). Off-Linux this reports 0 with no panic.
    pub fn start() -> Self {
        Self::start_with_source(|| ProcessMemory::capture().anon_rss_bytes)
    }

    /// Start a sampler over an injectable value-source returning the current
    /// anonymous RSS in bytes. The background thread calls
    /// [`StartupPeakRssSampler::sample_once`] once per [`POLL_INTERVAL`].
    ///
    /// The injected source makes the peak/phase logic deterministically
    /// unit-testable without reading `/proc`.
    pub fn start_with_source<F>(value_source: F) -> Self
    where
        F: Fn() -> u64 + Send + 'static,
    {
        let state = Arc::new(SamplerState::new());
        let thread_state = Arc::clone(&state);
        let handle = std::thread::Builder::new()
            .name("startup-rss-sampler".to_string())
            .spawn(move || {
                // Take an immediate sample so a short startup window still
                // records a non-zero peak before the first sleep elapses.
                loop {
                    sample_once_impl(&thread_state, &value_source);
                    if thread_state.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .expect("failed to spawn startup-rss-sampler thread");

        StartupPeakRssSampler {
            state,
            handle: Some(handle),
            stopped: false,
        }
    }

    /// Set the phase attributed to subsequent peak samples. Callable from any
    /// thread (lock-free single atomic store).
    pub fn set_phase(&self, phase: Phase) {
        self.state
            .current_phase
            .store(phase as u8, Ordering::Release);
    }

    /// The phase currently in effect (for tests / introspection).
    pub fn current_phase(&self) -> Phase {
        Phase::from_u8(self.state.current_phase.load(Ordering::Acquire))
    }

    /// Take a single poll step against the given value-source: read the current
    /// RSS, update the running max, and — only if the max was just raised —
    /// record the current phase as the peak-phase. Exposed for deterministic,
    /// non-racy unit tests; the background thread calls the same logic in a
    /// sleep loop.
    pub fn sample_once<F: Fn() -> u64>(&self, value_source: &F) {
        sample_once_impl(&self.state, value_source);
    }

    /// Current recorded peak in bytes.
    pub fn peak_bytes(&self) -> u64 {
        self.state.peak_bytes.load(Ordering::Acquire)
    }

    /// Phase recorded at the time the peak was reached.
    pub fn peak_phase(&self) -> Phase {
        Phase::from_u8(self.state.peak_phase.load(Ordering::Acquire))
    }

    /// Stop the sampler: signal shutdown, join the background thread, emit the
    /// greppable summary one-liner, and return the peak in bytes.
    ///
    /// Idempotent — a second call (or the `Drop` fallback) is a no-op that does
    /// not re-emit the log line or re-join.
    pub fn stop(&mut self) -> u64 {
        let peak = self.peak_bytes();
        if self.stopped {
            return peak;
        }
        self.stopped = true;

        self.state.stopping.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // join failure (thread panic) must not abort startup — the sampler
            // is observability-only.
            let _ = handle.join();
        }

        let peak = self.peak_bytes();
        let phase = self.peak_phase();
        // Emit as an integer field so it renders unquoted (`...=777`) for clean
        // grepping, mirroring the numeric memory-report fields.
        let peak_mb = (peak as f64 / (1024.0 * 1024.0)).round() as u64;
        info!(
            startup_peak_anon_rss_mb = peak_mb,
            phase = phase.as_str(),
            "Startup peak RSS summary"
        );
        peak
    }
}

impl Drop for StartupPeakRssSampler {
    fn drop(&mut self) {
        // Fallback: if stop() was never called, ensure the thread is joined so
        // it can never outlive the sampler. We do NOT emit the summary line
        // here — that is reserved for an explicit stop().
        self.state.stopping.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The single poll step, shared by the background thread and `sample_once`.
///
/// Reads the current RSS via the value-source, raises the running max, and —
/// only when the max was just raised — records the current phase as the
/// peak-phase. The current-phase atomic is read AFTER the max is confirmed to
/// have increased and only by the writer of the peak, so the recorded
/// peak/peak-phase pair is always self-consistent.
fn sample_once_impl<F: Fn() -> u64>(state: &SamplerState, value_source: &F) {
    let sample = value_source();
    let prev = state.peak_bytes.fetch_max(sample, Ordering::AcqRel);
    if sample > prev {
        let phase = state.current_phase.load(Ordering::Acquire);
        state.peak_phase.store(phase, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// A value-source driven by a fixed, in-order list of samples. Each call
    /// returns the next value (clamping at the last). Lets tests drive
    /// `sample_once` deterministically without a real `/proc` read or a racy
    /// free-running thread.
    fn scripted_source(samples: Vec<u64>) -> impl Fn() -> u64 {
        let idx = AtomicUsize::new(0);
        let samples = Arc::new(samples);
        move || {
            let i = idx.fetch_add(1, Ordering::SeqCst);
            let s = &samples;
            *s.get(i).or_else(|| s.last()).unwrap_or(&0)
        }
    }

    fn mb(v: u64) -> u64 {
        v * 1024 * 1024
    }

    /// A sampler with no background thread, for deterministic `sample_once`
    /// driving.
    fn idle_sampler() -> StartupPeakRssSampler {
        StartupPeakRssSampler::for_test()
    }

    #[test]
    fn test_peak_tracks_max_of_injected_samples() {
        let mut s = idle_sampler();
        let src = scripted_source(vec![mb(100), mb(500), mb(300), mb(450)]);
        for _ in 0..4 {
            s.sample_once(&src);
        }
        assert_eq!(s.peak_bytes(), mb(500), "peak must be the max sample");
        let _ = s.stop();
    }

    #[test]
    fn test_peak_records_phase_at_peak() {
        let mut s = idle_sampler();
        // bucket-apply @ 500 (the peak), then cache-scan @ 300 (below peak).
        s.set_phase(Phase::BucketApply);
        let src_hi = scripted_source(vec![mb(500)]);
        s.sample_once(&src_hi);
        s.set_phase(Phase::CacheScan);
        let src_lo = scripted_source(vec![mb(300)]);
        s.sample_once(&src_lo);
        assert_eq!(s.peak_bytes(), mb(500));
        assert_eq!(
            s.peak_phase(),
            Phase::BucketApply,
            "peak-phase must reflect the phase current when the max was set"
        );
        let _ = s.stop();
    }

    #[test]
    fn test_phase_transition_updates_current_phase() {
        let mut s = idle_sampler();
        assert_eq!(s.current_phase(), Phase::Startup);
        s.set_phase(Phase::BucketApply);
        assert_eq!(s.current_phase(), Phase::BucketApply);
        s.set_phase(Phase::CacheScan);
        assert_eq!(s.current_phase(), Phase::CacheScan);
        s.set_phase(Phase::Catchup);
        assert_eq!(s.current_phase(), Phase::Catchup);
        let _ = s.stop();
    }

    #[test]
    fn test_peak_zero_when_source_zero() {
        // Non-Linux / error path: value-source returns 0 throughout.
        let mut s = idle_sampler();
        let src = scripted_source(vec![0, 0, 0]);
        for _ in 0..3 {
            s.sample_once(&src);
        }
        assert_eq!(s.peak_bytes(), 0);
        // stop() must not panic and must report peak 0.
        assert_eq!(s.stop(), 0);
    }

    #[test]
    fn test_stop_idempotent() {
        let mut s = idle_sampler();
        let src = scripted_source(vec![mb(123)]);
        s.sample_once(&src);
        let first = s.stop();
        let second = s.stop();
        assert_eq!(first, mb(123));
        assert_eq!(second, mb(123), "second stop() returns the same peak");
        // Drop after stop() must not panic / double-join (covered by going out
        // of scope at end of test).
    }

    #[test]
    fn test_background_thread_joins_cleanly() {
        // Smoke test of the real threaded path with a non-zero source: stop()
        // must join without leaking the thread.
        let mut s = StartupPeakRssSampler::start_with_source(|| mb(42));
        // Give the thread a chance to take at least one sample.
        std::thread::sleep(Duration::from_millis(50));
        let peak = s.stop();
        assert!(peak >= mb(42), "thread should have sampled the source");
    }

    /// Drive `sample_once` across the cold-catchup restore sub-phase
    /// transitions and assert the peak is attributed to the sub-phase current
    /// when the running max was raised. Deterministic (no thread, no `/proc`).
    #[test]
    fn test_subphase_peak_attribution() {
        let mut s = idle_sampler();
        // live-restore @ 500MB, then merge-restart @ 700MB (the peak), then
        // cache-scan @ 300MB (below peak).
        s.set_phase(Phase::LiveBucketRestore);
        s.sample_once(&scripted_source(vec![mb(500)]));
        s.set_phase(Phase::MergeRestart);
        s.sample_once(&scripted_source(vec![mb(700)]));
        s.set_phase(Phase::CacheScan);
        s.sample_once(&scripted_source(vec![mb(300)]));
        assert_eq!(s.peak_bytes(), mb(700));
        assert_eq!(
            s.peak_phase(),
            Phase::MergeRestart,
            "peak must be attributed to the sub-phase current when the max was raised"
        );
        let _ = s.stop();
    }

    /// Every `log_startup_memory` checkpoint string must map to the intended
    /// `Phase` (guards against typos / missed strings).
    #[test]
    fn test_checkpoint_string_maps_to_phase() {
        let cases: &[(&str, Phase)] = &[
            ("before_restore_bucket_list", Phase::LiveBucketRestore),
            ("after_restore_bucket_list", Phase::MergeRestart),
            ("before_cache_scan", Phase::CacheScan),
            ("after_cache_scan", Phase::CacheScan),
            ("hot_archive_restore", Phase::HotArchiveRestore),
            ("after_cache_scan_and_merges", Phase::MergeRestart),
            ("after_verify_install_buckets", Phase::CacheInstall),
            ("after_bucket_cache_init", Phase::CacheInstall),
            ("after_cache_install", Phase::CacheInstall),
            ("after_post_catchup_cache_warm", Phase::PostCatchupWarm),
        ];
        for (s, expected) in cases {
            assert_eq!(
                phase_for_checkpoint(s),
                Some(*expected),
                "checkpoint string {s:?} must map to {expected:?}"
            );
        }
        // Unknown strings map to None (no-op, leaves current_phase unchanged).
        assert_eq!(phase_for_checkpoint("not_a_checkpoint"), None);
    }

    /// Every `Phase` variant must roundtrip through `from_u8(as u8)` and have a
    /// unique, stable `as_str()`.
    #[test]
    fn test_phase_enum_roundtrip() {
        let all = [
            Phase::Startup,
            Phase::BucketApply,
            Phase::CacheScan,
            Phase::Catchup,
            Phase::LiveBucketRestore,
            Phase::MergeRestart,
            Phase::HotArchiveRestore,
            Phase::CacheInstall,
            Phase::PostCatchupWarm,
        ];
        let mut seen = std::collections::HashSet::new();
        for p in all {
            assert_eq!(
                Phase::from_u8(p as u8),
                p,
                "{p:?} must roundtrip via from_u8"
            );
            assert!(
                seen.insert(p.as_str()),
                "as_str() for {p:?} ({}) collides with another variant",
                p.as_str()
            );
        }
    }

    /// `note_checkpoint` with no registered sampler must be a no-op and never
    /// panic.
    #[test]
    fn test_note_checkpoint_noop_without_registered_sampler() {
        clear_global_sampler();
        // Must not panic; nothing to observe (no registered sampler).
        note_checkpoint("before_restore_bucket_list");
        note_checkpoint("not_a_checkpoint");
    }

    /// Registering a sampler routes `note_checkpoint` into its `current_phase`;
    /// clearing the registration restores the no-op behavior.
    #[test]
    fn test_note_checkpoint_sets_registered_sampler_phase() {
        let _guard = GLOBAL_SAMPLER_TEST_LOCK.lock().unwrap();
        let s = idle_sampler();
        assert_eq!(s.current_phase(), Phase::Startup);
        register_global_sampler(&s);
        note_checkpoint("before_restore_bucket_list");
        assert_eq!(s.current_phase(), Phase::LiveBucketRestore);
        note_checkpoint("after_post_catchup_cache_warm");
        assert_eq!(s.current_phase(), Phase::PostCatchupWarm);
        // Unknown string leaves the phase unchanged.
        note_checkpoint("not_a_checkpoint");
        assert_eq!(s.current_phase(), Phase::PostCatchupWarm);
        // After clearing, note_checkpoint is a no-op again.
        clear_global_sampler();
        note_checkpoint("before_restore_bucket_list");
        assert_eq!(s.current_phase(), Phase::PostCatchupWarm);
    }

    /// Verify `stop()` emits the greppable `startup_peak_anon_rss_mb=` /
    /// `phase=` summary line, in both structured and Text-rendered form
    /// (mirrors the `memory_report` field tests).
    #[test]
    fn test_summary_line_format() {
        use std::io;
        use tracing::subscriber::with_default;
        use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

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

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = buf.clone();
        let fmt_layer = fmt::layer()
            .with_writer(move || -> Box<dyn io::Write> { Box::new(BufWriter(buf_clone.clone())) })
            .with_ansi(false)
            .with_target(true);
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(fmt_layer);

        with_default(subscriber, || {
            let mut s = idle_sampler();
            s.set_phase(Phase::BucketApply);
            let src = scripted_source(vec![mb(777)]);
            s.sample_once(&src);
            let _ = s.stop();
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("startup_peak_anon_rss_mb=777"),
            "summary line must render startup_peak_anon_rss_mb=<N>. Got: {output}"
        );
        assert!(
            output.contains("phase=\"bucket-apply\"") || output.contains("phase=bucket-apply"),
            "summary line must render the peak phase. Got: {output}"
        );
    }
}
