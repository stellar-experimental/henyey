//! Non-blocking cache for the latest archive checkpoint.
//!
//! The event-loop task must never block on an archive HTTP fetch. A hanging
//! TCP/DNS/TLS attempt can stall the loop for tens of seconds — see issue
//! #1784 for the mainnet freeze at `phase=13 buffered_catchup` where the
//! event loop sat in `get_cached_archive_checkpoint` waiting on
//! `HistoryArchive::fetch_root_has()` for up to 89 s.
//!
//! [`ArchiveCheckpointCache`] exposes a synchronous [`get_cached`] accessor
//! that returns immediately with a [`CacheResult`] — `Cold` (never populated),
//! `Stale` (TTL expired, stale value returned), or `Fresh` (within TTL) —
//! kicking off a background refresh via [`maybe_spawn_refresh`] when the cache
//! is cold or stale. Callers on the event loop must treat `Cold` as
//! "unknown — skip this tick"; the next recovery tick (10 s later) will see
//! the refreshed value.
//!
//! Off-loop callers (startup wait, catchup worker) use [`fetch_blocking`],
//! which awaits the underlying fetcher directly and may take up to
//! `retries × timeout` seconds.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use henyey_clock::Clock;
use henyey_history::{DownloadConfig, HistoryArchive};
use parking_lot::RwLock;

use crate::config::HistoryArchiveEntry;
use crate::metrics::ARCHIVE_CACHE_REFRESH_DURATION_SECONDS;

/// How long to cache the archive checkpoint before triggering a background
/// refresh. Matches the previous `ARCHIVE_CHECKPOINT_CACHE_SECS` constant.
///
/// This is the mainnet-tuned value (checkpoint_frequency=64, ~5 min per
/// checkpoint). In accelerated mode (checkpoint_frequency=8, ~8s per
/// checkpoint) it is scaled down by [`cache_ttl_secs`] so the cache does
/// not serve values that are 7+ checkpoints out of date.
pub(super) const ARCHIVE_CHECKPOINT_CACHE_SECS: u64 = 60;

/// Reduced cache TTL used when the node is archive-dependent (urgent mode —
/// see [`ArchiveCheckpointCache::set_urgent`]).  Activated whenever the archive
/// is confirmed behind the next checkpoint, regardless of peer tx_set state
/// (#2073).  Set to one recovery-timer tick so the cache refreshes on every
/// cycle.
pub(super) const ARCHIVE_CHECKPOINT_CACHE_URGENT_SECS: u64 = 10;

/// Return the effective cache TTL given the current checkpoint frequency.
///
/// In accelerated mode the archive is localhost and publishes checkpoints
/// rapidly — during the primary's rapid-close phase a new checkpoint can
/// arrive every ~1s. A stale cache blocks captive-core from seeing these
/// fresh checkpoints for the duration of the TTL, stacking with the
/// archive-behind backoff into a multi-second dead window where catchup
/// cannot progress. Use an aggressive 1s TTL in accelerated mode; the
/// archive fetch against localhost costs < 10 ms so frequent refreshes
/// are essentially free.
pub(super) fn cache_ttl_secs() -> u64 {
    let freq = henyey_history::checkpoint::checkpoint_frequency();
    let default_freq = henyey_history::DEFAULT_CHECKPOINT_FREQUENCY;
    if freq < default_freq {
        1
    } else {
        ARCHIVE_CHECKPOINT_CACHE_SECS
    }
}

/// Hard ceiling on a single background refresh. Three recovery ticks
/// (3 × `OUT_OF_SYNC_RECOVERY_TIMER_SECS`) — if the refresh hasn't
/// completed in that window, treating the next tick as "archive unreachable"
/// and falling back to peer-SCP is the correct conservative behavior.
pub(super) const ARCHIVE_REFRESH_TIMEOUT_SECS: u64 = 30;

/// Per-archive timeout and retry count for the *background refresh* path.
///
/// Deliberately tighter than the default [`DownloadConfig`] used by the
/// catchup-time fetches: a refresh's job is "get a fresh answer or give up
/// fast", not "must eventually succeed". The outer
/// [`ARCHIVE_REFRESH_TIMEOUT_SECS`] cancels the whole fetch if the inner
/// iteration exceeds the ceiling, so this purely shortens the common
/// single-archive-timeout case.
const REFRESH_INNER_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_INNER_RETRIES: u32 = 1;
const REFRESH_INNER_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Abstraction over "query the configured archives for the latest
/// checkpoint". The production impl [`ArchiveHttpFetcher`] does the actual
/// HTTP work; tests inject a controllable mock that can hang, error, or
/// return specific values on demand.
#[async_trait]
pub(super) trait ArchiveCheckpointFetcher: Send + Sync {
    async fn fetch(&self) -> anyhow::Result<u32>;
}

/// Production fetcher: iterates configured archives serially and returns
/// the latest published checkpoint from the first archive that responds.
///
/// This mirrors the previous inline logic in `App::get_latest_checkpoint`.
/// Archive list and per-request HTTP config are captured at construction so
/// the cache does not need a reference back to `App`.
pub(super) struct ArchiveHttpFetcher {
    archives: Vec<HistoryArchiveEntry>,
    download_config: DownloadConfig,
}

impl ArchiveHttpFetcher {
    pub(super) fn new(archives: Vec<HistoryArchiveEntry>, download_config: DownloadConfig) -> Self {
        Self {
            archives,
            download_config,
        }
    }

    /// Build a fetcher suitable for the background-refresh path:
    /// tighter retries/timeout so a single slow archive doesn't dominate
    /// the 30 s refresh ceiling.
    pub(super) fn for_background_refresh(archives: Vec<HistoryArchiveEntry>) -> Self {
        Self::new(
            archives,
            DownloadConfig {
                timeout: REFRESH_INNER_TIMEOUT,
                retries: REFRESH_INNER_RETRIES,
                retry_delay: REFRESH_INNER_RETRY_DELAY,
            },
        )
    }

    /// Build a fetcher suitable for the blocking/catchup-time path, using
    /// the full default retry/timeout budget.
    pub(super) fn for_blocking_catchup(archives: Vec<HistoryArchiveEntry>) -> Self {
        Self::new(archives, DownloadConfig::default())
    }
}

#[async_trait]
impl ArchiveCheckpointFetcher for ArchiveHttpFetcher {
    async fn fetch(&self) -> anyhow::Result<u32> {
        tracing::info!("Querying history archives for latest checkpoint");

        for archive_config in &self.archives {
            if !archive_config.get_enabled {
                continue;
            }
            match HistoryArchive::with_config(&archive_config.url, self.download_config.clone()) {
                Ok(archive) => match archive.fetch_current_ledger().await {
                    Ok(ledger) => {
                        tracing::info!(
                            ledger,
                            archive = %archive_config.url,
                            "Got current ledger from archive"
                        );
                        match henyey_history::checkpoint::latest_checkpoint_before_or_at(ledger) {
                            Some(checkpoint) => return Ok(checkpoint),
                            None => {
                                tracing::info!(ledger, "Archive has no completed checkpoint yet");
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            archive = %archive_config.url,
                            error = %e,
                            "Failed to get current ledger from archive"
                        );
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        archive = %archive_config.url,
                        error = %e,
                        "Failed to create archive client"
                    );
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!("No checkpoint available from any archive"))
    }
}

/// Result of a non-blocking cache read, distinguishing cold (never
/// populated), stale (TTL expired, background refresh spawned), and
/// fresh (within TTL) states.
///
/// Callers should exhaustively match all three variants rather than
/// collapsing `Fresh`/`Stale` into a single arm — this strongly encourages
/// handling the cold state explicitly and prevents the class of bug where
/// `set_urgent(true)` is forgotten on cold cache (#1864).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheResult {
    /// Cache never populated; background refresh spawned.
    Cold,
    /// TTL expired; stale value returned, background refresh spawned.
    Stale(u32),
    /// Within TTL; value is current.
    Fresh(u32),
}

/// Snapshot of a cached archive-checkpoint observation.
#[derive(Debug, Clone, Copy)]
struct CachedCheckpoint {
    checkpoint: u32,
    queried_at: Instant,
}

/// Non-blocking cache wrapper.
///
/// The inner `RwLock` is a [`parking_lot::RwLock`] — cheap and strictly
/// non-async. Event-loop callers pay no `.await` cost to observe the cache.
pub(super) struct ArchiveCheckpointCache {
    value: RwLock<Option<CachedCheckpoint>>,
    refreshing: AtomicBool,
    /// When true, the effective TTL is reduced to
    /// [`ARCHIVE_CHECKPOINT_CACHE_URGENT_SECS`] so the node detects
    /// a freshly-published checkpoint within one recovery tick (~10 s).
    /// Set by `trigger_recovery_catchup` when the archive is the only
    /// recovery path (peers' tx_sets evicted); cleared on progress or
    /// after successful catchup.  See issue #1847.
    urgent: AtomicBool,
    stale_returns: AtomicU64,
    cold_returns: AtomicU64,
    fresh_returns: AtomicU64,
    refresh_timeouts: AtomicU64,
    refresh_errors: AtomicU64,
    refresh_successes: AtomicU64,
    clock: Arc<dyn Clock>,
    /// Fetcher used for background refreshes. Test seams replace this with
    /// a [`MockFetcher`].
    background_fetcher: RwLock<Arc<dyn ArchiveCheckpointFetcher>>,
}

impl ArchiveCheckpointCache {
    pub(super) fn new(
        clock: Arc<dyn Clock>,
        background_fetcher: Arc<dyn ArchiveCheckpointFetcher>,
    ) -> Self {
        Self {
            value: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            urgent: AtomicBool::new(false),
            stale_returns: AtomicU64::new(0),
            cold_returns: AtomicU64::new(0),
            fresh_returns: AtomicU64::new(0),
            refresh_timeouts: AtomicU64::new(0),
            refresh_errors: AtomicU64::new(0),
            refresh_successes: AtomicU64::new(0),
            clock,
            background_fetcher: RwLock::new(background_fetcher),
        }
    }

    /// Return the latest cached archive checkpoint without blocking.
    ///
    /// Returns [`CacheResult::Cold`] when the cache has never been populated,
    /// [`CacheResult::Stale`] when the TTL has expired (stale value returned,
    /// background refresh spawned), or [`CacheResult::Fresh`] when the value
    /// is within the TTL.
    ///
    /// Event-loop callers MUST treat `Cold` as "unknown — skip this tick".
    pub(super) fn get_cached(self: &Arc<Self>) -> CacheResult {
        let effective_ttl = self.effective_ttl_secs();
        let (value, needs_refresh) = {
            let guard = self.value.read();
            match *guard {
                Some(c) => {
                    let age = self.clock.now().saturating_duration_since(c.queried_at);
                    let stale = age.as_secs() >= effective_ttl;
                    (Some(c.checkpoint), stale)
                }
                None => (None, true),
            }
        };

        let result = match (value, needs_refresh) {
            (None, _) => {
                self.cold_returns.fetch_add(1, Ordering::Relaxed);
                CacheResult::Cold
            }
            (Some(v), true) => {
                self.stale_returns.fetch_add(1, Ordering::Relaxed);
                CacheResult::Stale(v)
            }
            (Some(v), false) => {
                self.fresh_returns.fetch_add(1, Ordering::Relaxed);
                CacheResult::Fresh(v)
            }
        };

        if needs_refresh {
            self.maybe_spawn_refresh();
        }

        result
    }

    /// Await a fresh fetch. Acceptable callers only: startup
    /// (`wait_for_archive_checkpoint`) and spawned catchup tasks
    /// (`run_catchup_work`). Must NOT be called from the event-loop task.
    pub(super) async fn fetch_blocking<F>(&self, fetcher: &F) -> anyhow::Result<u32>
    where
        F: ArchiveCheckpointFetcher + ?Sized,
    {
        let checkpoint = fetcher.fetch().await?;
        *self.value.write() = Some(CachedCheckpoint {
            checkpoint,
            queried_at: self.clock.now(),
        });
        Ok(checkpoint)
    }

    /// Overwrite the cached value with a fresh timestamp. Used by tests to
    /// pre-warm the cache without going through the HTTP fetcher.
    #[cfg(test)]
    pub(super) fn seed(&self, checkpoint: u32) {
        *self.value.write() = Some(CachedCheckpoint {
            checkpoint,
            queried_at: self.clock.now(),
        });
    }

    /// Seed the cache with a checkpoint value that's immediately considered
    /// stale. The value is available for reading, but the next `get_cached()`
    /// call will trigger a background refresh to discover any newer checkpoint.
    ///
    /// Used after catchup: the caught-up checkpoint provides a baseline for
    /// recovery paths, but we want to immediately discover any checkpoint
    /// published during the catchup window without waiting for the normal
    /// 60s TTL to expire.
    ///
    /// Monotonic: only writes if `checkpoint >= current_cached_value`.
    /// Equal checkpoints are written to ensure the timestamp is marked stale
    /// (the common post-catchup case where the cache already holds the
    /// caught-up checkpoint from a prior blocking fetch).
    pub(super) fn seed_stale(&self, checkpoint: u32) {
        let mut guard = self.value.write();
        let should_write = match *guard {
            Some(c) => checkpoint >= c.checkpoint,
            None => true,
        };
        if should_write {
            // Use a `queried_at` far enough in the past that the cache
            // is immediately stale regardless of effective TTL mode
            // (normal=60s, urgent=10s, accelerated=1s).
            let ttl_duration = Duration::from_secs(ARCHIVE_CHECKPOINT_CACHE_SECS * 2);
            let now = self.clock.now();
            let stale_time = now.checked_sub(ttl_duration).unwrap_or(now);
            *guard = Some(CachedCheckpoint {
                checkpoint,
                queried_at: stale_time,
            });
        }
    }

    /// Test / setup hook: overwrite the cached value with an explicit
    /// `queried_at` — used to simulate a stale cache without `tokio::time::pause`.
    #[cfg(test)]
    pub(super) fn seed_with_queried_at(&self, checkpoint: u32, queried_at: Instant) {
        *self.value.write() = Some(CachedCheckpoint {
            checkpoint,
            queried_at,
        });
    }

    /// Clear the cached value.
    ///
    /// Historically called by `trigger_recovery_catchup` to force a
    /// cold non-blocking read (see `consensus.rs` comment dated
    /// 2026-04-18 for why that was removed — net-negative behavior). No
    /// production caller remains; this is retained for the existing
    /// regression tests that need to exercise the cold-cache branch of
    /// `get_cached()`.
    #[cfg(test)]
    pub(super) fn clear(&self) {
        *self.value.write() = None;
    }

    /// Enable or disable urgent mode.
    ///
    /// When urgent, the effective cache TTL is reduced to
    /// [`ARCHIVE_CHECKPOINT_CACHE_URGENT_SECS`] so background refreshes
    /// fire on every recovery tick (~10 s) instead of every 60 s.
    ///
    /// Activated by:
    /// - `trigger_recovery_catchup` when the archive is confirmed behind
    ///   the next checkpoint (Fresh/Stale branch)
    /// - `escalate_recovery_to_catchup` (coupled with escalation counter)
    /// - catchup validation paths when the archive is confirmed behind
    ///
    /// Cleared on ledger progress or successful catchup.
    ///
    /// Prior to #2073 this was gated on `tx_set_all_peers_exhausted`,
    /// which delayed activation by up to 60 s (one full normal-TTL
    /// cycle).  It is now set unconditionally on any archive-behind
    /// signal.
    pub(super) fn set_urgent(&self, urgent: bool) {
        self.urgent.store(urgent, Ordering::Relaxed);
    }

    /// Whether urgent mode is currently active.
    pub(super) fn is_urgent(&self) -> bool {
        self.urgent.load(Ordering::Relaxed)
    }

    /// The effective TTL in seconds: urgent mode wins over normal mode,
    /// and accelerated mode (checkpoint_frequency < 64) always uses 1 s.
    fn effective_ttl_secs(&self) -> u64 {
        let base = cache_ttl_secs();
        if base <= ARCHIVE_CHECKPOINT_CACHE_URGENT_SECS {
            // Accelerated mode already uses an aggressive TTL.
            base
        } else if self.urgent.load(Ordering::Relaxed) {
            ARCHIVE_CHECKPOINT_CACHE_URGENT_SECS
        } else {
            base
        }
    }

    /// Age of the cached value, or `None` if the cache is cold.
    /// Used for observability logging in `trigger_recovery_catchup`.
    pub(super) fn last_query_age(&self) -> Option<Duration> {
        self.value
            .read()
            .map(|c| self.clock.now().saturating_duration_since(c.queried_at))
    }

    /// Test hook: replace the background fetcher. Production code never
    /// calls this; it is used by unit tests that need to inject a mock.
    #[cfg(test)]
    pub(super) fn set_background_fetcher(&self, fetcher: Arc<dyn ArchiveCheckpointFetcher>) {
        *self.background_fetcher.write() = fetcher;
    }

    pub(super) fn stale_returns(&self) -> u64 {
        self.stale_returns.load(Ordering::Relaxed)
    }

    pub(super) fn cold_returns(&self) -> u64 {
        self.cold_returns.load(Ordering::Relaxed)
    }

    pub(super) fn fresh_returns(&self) -> u64 {
        self.fresh_returns.load(Ordering::Relaxed)
    }

    pub(super) fn refresh_timeouts(&self) -> u64 {
        self.refresh_timeouts.load(Ordering::Relaxed)
    }

    pub(super) fn refresh_errors(&self) -> u64 {
        self.refresh_errors.load(Ordering::Relaxed)
    }

    pub(super) fn refresh_successes(&self) -> u64 {
        self.refresh_successes.load(Ordering::Relaxed)
    }

    /// Whether the cache has been populated at least once.
    pub(super) fn is_populated(&self) -> bool {
        self.value.read().is_some()
    }

    #[cfg(test)]
    pub(super) fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::Acquire)
    }

    fn maybe_spawn_refresh(self: &Arc<Self>) {
        let Some(guard) = RefreshGuard::try_acquire(self) else {
            tracing::debug!("Archive checkpoint refresh already in flight; not spawning another");
            return;
        };

        let this = Arc::clone(self);
        henyey_common::spawn_observed("archive_checkpoint_refresh", async move {
            let _guard = guard;
            let fetcher = Arc::clone(&*this.background_fetcher.read());
            let fut = async { fetcher.fetch().await };
            let refresh_start = Instant::now();
            let outcome =
                tokio::time::timeout(Duration::from_secs(ARCHIVE_REFRESH_TIMEOUT_SECS), fut).await;

            match outcome {
                Ok(Ok(checkpoint)) => {
                    metrics::histogram!(ARCHIVE_CACHE_REFRESH_DURATION_SECONDS)
                        .record(refresh_start.elapsed().as_secs_f64());
                    *this.value.write() = Some(CachedCheckpoint {
                        checkpoint,
                        queried_at: this.clock.now(),
                    });
                    this.refresh_successes.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(checkpoint, "Archive checkpoint refresh succeeded");
                }
                Ok(Err(e)) => {
                    metrics::histogram!(ARCHIVE_CACHE_REFRESH_DURATION_SECONDS)
                        .record(refresh_start.elapsed().as_secs_f64());
                    this.refresh_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        error = %e,
                        "Archive checkpoint refresh failed"
                    );
                }
                Err(_) => {
                    // Record the full timeout duration — this is the actual stall.
                    metrics::histogram!(ARCHIVE_CACHE_REFRESH_DURATION_SECONDS)
                        .record(ARCHIVE_REFRESH_TIMEOUT_SECS as f64);
                    this.refresh_timeouts.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        timeout_secs = ARCHIVE_REFRESH_TIMEOUT_SECS,
                        "Archive checkpoint refresh timed out"
                    );
                }
            }
        });
    }
}

/// RAII guard that clears the `refreshing` flag on drop, ensuring the flag is
/// released even if the refresh task panics or is cancelled.
struct RefreshGuard {
    cache: Arc<ArchiveCheckpointCache>,
}

impl RefreshGuard {
    /// Attempt to acquire the refresh permit via CAS. Returns `Some(Self)` if
    /// the flag was successfully set from `false` to `true`.
    fn try_acquire(cache: &Arc<ArchiveCheckpointCache>) -> Option<Self> {
        if cache
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(Self {
                cache: Arc::clone(cache),
            })
        } else {
            None
        }
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.cache.refreshing.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;

    struct MockFetcher {
        call_count: AtomicUsize,
        response: Arc<MockResponse>,
    }

    enum MockResponse {
        /// Immediately return `Ok(checkpoint)`.
        Ok(u32),
        /// Immediately return `Err(...)`.
        Err,
        /// Block on a `Notify` before returning `Ok(checkpoint)`. Use to
        /// simulate a hanging archive.
        BlockThenOk { gate: Arc<Notify>, checkpoint: u32 },
        /// Block forever. Use to exercise the refresh timeout.
        Hang,
        /// Panic inside the fetch call. Use to exercise the RAII guard.
        Panic,
    }

    #[async_trait]
    impl ArchiveCheckpointFetcher for MockFetcher {
        async fn fetch(&self) -> anyhow::Result<u32> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match &*self.response {
                MockResponse::Ok(v) => Ok(*v),
                MockResponse::Err => Err(anyhow::anyhow!("mock error")),
                MockResponse::BlockThenOk { gate, checkpoint } => {
                    gate.notified().await;
                    Ok(*checkpoint)
                }
                MockResponse::Hang => {
                    // Wait on a Notify that nobody ever signals.
                    let gate = Notify::new();
                    gate.notified().await;
                    unreachable!("gate should never fire")
                }
                MockResponse::Panic => {
                    panic!("simulated panic in refresh task")
                }
            }
        }
    }

    fn mk_cache(response: MockResponse) -> (Arc<ArchiveCheckpointCache>, Arc<MockFetcher>) {
        let clock: Arc<dyn Clock> = Arc::new(henyey_clock::RealClock);
        let fetcher = Arc::new(MockFetcher {
            call_count: AtomicUsize::new(0),
            response: Arc::new(response),
        });
        let cache = Arc::new(ArchiveCheckpointCache::new(
            clock,
            fetcher.clone() as Arc<dyn ArchiveCheckpointFetcher>,
        ));
        (cache, fetcher)
    }

    /// Cache hit on a warm value returns immediately and does NOT spawn a
    /// refresh.
    #[tokio::test]
    async fn test_cache_hit_immediate_no_spawn() {
        let (cache, fetcher) = mk_cache(MockResponse::Ok(1234));
        cache.seed(999);

        let before = cache.stale_returns();
        let got = cache.get_cached();
        assert_eq!(
            got,
            CacheResult::Fresh(999),
            "fresh cache returns seeded value"
        );
        assert_eq!(
            cache.stale_returns(),
            before,
            "fresh cache must not count as stale"
        );

        // Give any (incorrectly) spawned task a chance to run.
        tokio::task::yield_now().await;
        assert_eq!(
            fetcher.call_count.load(Ordering::SeqCst),
            0,
            "fresh cache must not spawn a refresh"
        );
    }

    /// Cold cache returns `None` without blocking on the background refresh
    /// and triggers exactly one background fetch.
    #[tokio::test]
    async fn test_cold_cache_returns_none_and_spawns() {
        let gate = Arc::new(Notify::new());
        let (cache, fetcher) = mk_cache(MockResponse::BlockThenOk {
            gate: gate.clone(),
            checkpoint: 77,
        });

        // get_cached() is synchronous — it returns immediately without
        // awaiting the background refresh. The ordering proof is:
        // got == None (not the fetched 77) + is_refreshing() + call_count == 1.
        let got = cache.get_cached();
        assert_eq!(got, CacheResult::Cold, "cold cache returns Cold");
        assert_eq!(cache.cold_returns(), 1, "cold return counter incremented");

        // Observe that exactly one refresh is in flight.
        // Yield so the spawned task gets a chance to start.
        tokio::task::yield_now().await;
        assert_eq!(
            fetcher.call_count.load(Ordering::SeqCst),
            1,
            "one refresh spawned"
        );
        assert!(cache.is_refreshing(), "refreshing flag is set");

        // Let the refresh finish.
        gate.notify_one();
        // Drain the spawned task.
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!cache.is_refreshing(), "refresh completed");
        assert_eq!(cache.refresh_successes(), 1);

        // Subsequent call returns the fetched value.
        let got = cache.get_cached();
        assert_eq!(got, CacheResult::Fresh(77));
        assert_eq!(cache.fresh_returns(), 1, "fresh return counter incremented");
    }

    /// Concurrent callers on a cold cache spawn exactly one refresh.
    #[tokio::test]
    async fn test_concurrent_callers_single_refresh() {
        let gate = Arc::new(Notify::new());
        let (cache, fetcher) = mk_cache(MockResponse::BlockThenOk {
            gate: gate.clone(),
            checkpoint: 42,
        });

        // Fire 10 callers concurrently.
        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = Arc::clone(&cache);
            handles.push(tokio::spawn(async move { c.get_cached() }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), CacheResult::Cold);
        }

        // Yield so the spawned refresh runs.
        tokio::task::yield_now().await;
        assert_eq!(
            fetcher.call_count.load(Ordering::SeqCst),
            1,
            "only one fetch spawned despite 10 concurrent callers"
        );

        gate.notify_one();
    }

    /// Stale cache returns the stale value immediately and spawns a refresh
    /// that delivers the new value.
    #[tokio::test]
    async fn test_stale_cache_returns_stale_and_refreshes() {
        let (cache, fetcher) = mk_cache(MockResponse::Ok(200));
        // Seed with a stale timestamp (twice the cache TTL ago).
        let stale_at = Instant::now()
            .checked_sub(Duration::from_secs(2 * ARCHIVE_CHECKPOINT_CACHE_SECS))
            .expect("Instant::now - 120s should not underflow");
        cache.seed_with_queried_at(100, stale_at);

        let got = cache.get_cached();
        assert_eq!(
            got,
            CacheResult::Stale(100),
            "stale cache returns the stale value"
        );
        assert_eq!(cache.stale_returns(), 1);

        // Refresh should complete — drain.
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(fetcher.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(cache.refresh_successes(), 1);

        // Subsequent call returns the fresh value.
        let got = cache.get_cached();
        assert_eq!(got, CacheResult::Fresh(200));
    }

    /// Refresh timeout fires, clears `refreshing`, and increments the
    /// timeout counter. Uses a hanging fetcher and `tokio::time::pause` so
    /// the test runs in ~0 ms of real time.
    #[tokio::test(start_paused = true)]
    async fn test_refresh_timeout_clears_refreshing() {
        let (cache, _fetcher) = mk_cache(MockResponse::Hang);

        let got = cache.get_cached();
        assert_eq!(got, CacheResult::Cold);
        // Give the spawned task a chance to start and register the
        // timeout future with the paused runtime.
        tokio::task::yield_now().await;
        assert!(cache.is_refreshing());

        // Advance past the refresh timeout. With `tokio::time::pause()`
        // time is auto-advanced by `sleep`/`timeout` calls, so we just
        // need to drive the runtime past the deadline.
        tokio::time::advance(Duration::from_secs(ARCHIVE_REFRESH_TIMEOUT_SECS + 1)).await;
        // Drain until the cleanup block after the timeout runs.
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            !cache.is_refreshing(),
            "refreshing flag cleared after timeout"
        );
        assert_eq!(cache.refresh_timeouts(), 1);
        assert_eq!(cache.refresh_successes(), 0);
    }

    /// After a refresh error, subsequent calls may spawn another refresh
    /// (we must not lock the flag on error).
    #[tokio::test]
    async fn test_refresh_error_allows_retry() {
        let (cache, _fetcher) = mk_cache(MockResponse::Err);

        cache.get_cached(); // spawns refresh 1
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(cache.refresh_errors(), 1);
        assert!(!cache.is_refreshing());

        cache.get_cached(); // should spawn refresh 2
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(cache.refresh_errors(), 2);
    }

    /// Urgent mode reduces the effective TTL, causing the cache to
    /// spawn background refreshes more frequently.
    #[tokio::test]
    async fn test_urgent_mode_reduces_ttl() {
        let (cache, fetcher) = mk_cache(MockResponse::Ok(200));

        // Seed with a value 15 seconds ago (> urgent TTL, < normal TTL).
        let queried_at = std::time::Instant::now() - Duration::from_secs(15);
        cache.seed_with_queried_at(100, queried_at);

        // Normal mode: 15s < 60s TTL → not stale, no refresh.
        assert!(!cache.is_urgent());
        let stale_before = cache.stale_returns();
        let val = cache.get_cached();
        assert_eq!(val, CacheResult::Fresh(100));
        assert_eq!(
            cache.stale_returns(),
            stale_before,
            "should NOT be stale in normal mode"
        );

        // Enable urgent mode: 15s > 10s urgent TTL → stale, triggers refresh.
        cache.set_urgent(true);
        assert!(cache.is_urgent());
        let stale_before = cache.stale_returns();
        let val = cache.get_cached();
        assert_eq!(
            val,
            CacheResult::Stale(100),
            "returns stale value immediately"
        );
        assert_eq!(
            cache.stale_returns(),
            stale_before + 1,
            "should be stale in urgent mode"
        );

        // Wait for refresh to complete.
        for _ in 0..100 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            fetcher.call_count.load(Ordering::SeqCst) >= 1,
            "urgent mode should have triggered a refresh"
        );

        // Disable urgent mode.
        cache.set_urgent(false);
        assert!(!cache.is_urgent());
    }

    /// `last_query_age` returns `None` for a cold cache and `Some(age)`
    /// for a warm cache.
    #[tokio::test]
    async fn test_last_query_age() {
        let (cache, _fetcher) = mk_cache(MockResponse::Ok(100));

        // Cold cache → None.
        assert!(cache.last_query_age().is_none());

        // Seed with a known age.
        let queried_at = std::time::Instant::now() - Duration::from_secs(42);
        cache.seed_with_queried_at(100, queried_at);
        let age = cache.last_query_age().expect("should have age");
        // Allow 1s tolerance for test execution time.
        assert!(
            age.as_secs() >= 41 && age.as_secs() <= 44,
            "expected ~42s, got {:?}",
            age
        );

        // Fresh seed → small age.
        cache.seed(200);
        let age = cache.last_query_age().expect("should have age");
        assert!(
            age.as_secs() < 2,
            "fresh seed should have small age, got {:?}",
            age
        );
    }

    /// `seed_stale` makes the cache value available but immediately stale,
    /// so the next `get_cached()` triggers a background refresh.
    #[tokio::test]
    async fn test_seed_stale_triggers_refresh_on_next_get_cached() {
        let newer_checkpoint = 200u32;
        let (cache, fetcher) = mk_cache(MockResponse::Ok(newer_checkpoint));

        // Seed stale with an older checkpoint (simulates post-catchup state).
        cache.seed_stale(128);

        // The value should be readable.
        let val = cache.get_cached();
        assert_eq!(
            val,
            CacheResult::Stale(128),
            "seed_stale value should be readable"
        );

        // get_cached should have counted it as stale and spawned a refresh.
        assert_eq!(cache.stale_returns(), 1, "should be counted as stale");

        // Wait for the background refresh to complete.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fetcher.call_count.load(Ordering::SeqCst),
            1,
            "refresh should have been spawned"
        );
        // After refresh, the cache should contain the newer value from fetcher.
        let refreshed = cache.get_cached();
        assert_eq!(
            refreshed,
            CacheResult::Fresh(newer_checkpoint),
            "cache should be updated to fetcher's value after refresh"
        );
    }

    /// `seed_stale` is monotonic — it never regresses the cached value.
    #[tokio::test]
    async fn test_seed_stale_is_monotonic() {
        let (cache, _fetcher) = mk_cache(MockResponse::Ok(999));

        // Seed with a higher value first.
        cache.seed_stale(200);
        assert_eq!(cache.get_cached(), CacheResult::Stale(200));

        // Attempt to seed with a lower value — should be ignored.
        cache.seed_stale(100);
        assert_eq!(
            cache.get_cached(),
            CacheResult::Stale(200),
            "seed_stale must not regress the cached value"
        );

        // Seed with a higher value — should succeed.
        cache.seed_stale(300);
        assert_eq!(
            cache.get_cached(),
            CacheResult::Stale(300),
            "seed_stale should accept a higher value"
        );
    }

    /// `seed_stale` with the same checkpoint value still marks it stale,
    /// ensuring a refresh is triggered even when catchup completes at
    /// the same checkpoint already in the cache (the common case).
    #[tokio::test]
    async fn test_seed_stale_same_checkpoint_marks_stale() {
        let (cache, _fetcher) = mk_cache(MockResponse::Ok(999));

        // Seed fresh with checkpoint 200 (simulates a prior blocking fetch).
        cache.seed(200);
        // Verify it's fresh (no refresh triggered on next read).
        let val = cache.get_cached();
        assert_eq!(val, CacheResult::Fresh(200));

        // Now seed_stale with the same value (simulates post-catchup seeding).
        cache.seed_stale(200);
        // The value should still be readable.
        assert_eq!(cache.get_cached(), CacheResult::Stale(200));

        // The key assertion: the cache should now be stale, meaning a
        // background refresh was spawned. We verify by checking the
        // stale_returns counter (incremented when get_cached returns a
        // stale value).
        let stale_before = cache
            .stale_returns
            .load(std::sync::atomic::Ordering::Relaxed);
        let _ = cache.get_cached();
        let stale_after = cache
            .stale_returns
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            stale_after > stale_before,
            "seed_stale with equal checkpoint must mark cache as stale"
        );
    }

    /// `seed_stale` on a cold cache populates it with the stale value.
    #[tokio::test]
    async fn test_seed_stale_on_cold_cache() {
        // Use a fetcher that returns a value lower than what we'll seed,
        // so the monotonic guard doesn't interfere.
        let (cache, _fetcher) = mk_cache(MockResponse::Ok(50));

        // Cache starts cold.
        assert_eq!(cache.get_cached(), CacheResult::Cold);

        // Seed with a value higher than the fetcher returns.
        cache.seed_stale(128);
        let val = cache.get_cached();
        assert_eq!(
            val,
            CacheResult::Stale(128),
            "seed_stale should populate a cold cache"
        );
    }

    /// RefreshGuard clears the `refreshing` flag even when the refresh task
    /// panics, and a subsequent refresh can run successfully.
    #[tokio::test]
    async fn test_refresh_guard_cleared_on_panic() {
        let (cache, fetcher) = mk_cache(MockResponse::Panic);

        // Trigger a refresh — the task will panic inside fetch().
        cache.maybe_spawn_refresh();

        // Wait for the spawned task to complete (panic + unwind + guard drop).
        for _ in 0..200 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !cache.is_refreshing(),
            "refreshing flag must be cleared after panic"
        );
        assert_eq!(
            fetcher.call_count.load(Ordering::SeqCst),
            1,
            "first refresh task did execute"
        );

        // Replace the fetcher with one that succeeds.
        let ok_fetcher = Arc::new(MockFetcher {
            call_count: AtomicUsize::new(0),
            response: Arc::new(MockResponse::Ok(42)),
        });
        cache.set_background_fetcher(ok_fetcher.clone() as Arc<dyn ArchiveCheckpointFetcher>);

        // Trigger a second refresh — must succeed now that the flag is cleared.
        cache.maybe_spawn_refresh();

        for _ in 0..200 {
            if !cache.is_refreshing() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!cache.is_refreshing(), "second refresh completed");
        assert_eq!(
            ok_fetcher.call_count.load(Ordering::SeqCst),
            1,
            "second refresh task executed"
        );
        assert_eq!(
            cache.get_cached(),
            CacheResult::Fresh(42),
            "cache updated by second refresh"
        );
    }
}
