//! Non-blocking cache for the latest archive checkpoint.
//!
//! The event-loop task must never block on an archive HTTP fetch. A hanging
//! TCP/DNS/TLS attempt can stall the loop for tens of seconds — see issue
//! #1784 for the mainnet freeze at `phase=13 buffered_catchup` where the
//! event loop sat in `get_cached_archive_checkpoint` waiting on
//! `HistoryArchive::fetch_root_has()` for up to 89 s.
//!
//! [`ArchiveCheckpointCache`] exposes a synchronous [`get_cached`] accessor
//! that returns immediately, kicking off a background refresh via
//! [`maybe_spawn_refresh`] when the cache is cold or older than
//! [`ARCHIVE_CHECKPOINT_CACHE_SECS`]. Callers on the event loop must treat
//! `None` as "unknown — skip this tick"; the next recovery tick (10 s later)
//! will see the refreshed value.
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

/// How long to cache the archive checkpoint before triggering a background
/// refresh. Matches the previous `ARCHIVE_CHECKPOINT_CACHE_SECS` constant.
pub(super) const ARCHIVE_CHECKPOINT_CACHE_SECS: u64 = 60;

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
    stale_returns: AtomicU64,
    cold_returns: AtomicU64,
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
            stale_returns: AtomicU64::new(0),
            cold_returns: AtomicU64::new(0),
            refresh_timeouts: AtomicU64::new(0),
            refresh_errors: AtomicU64::new(0),
            refresh_successes: AtomicU64::new(0),
            clock,
            background_fetcher: RwLock::new(background_fetcher),
        }
    }

    /// Return the latest cached archive checkpoint without blocking.
    ///
    /// If the cache is cold or older than
    /// [`ARCHIVE_CHECKPOINT_CACHE_SECS`], spawn (at most one) background
    /// refresh and return the current (stale or `None`) value immediately.
    ///
    /// Event-loop callers MUST treat `None` as "unknown — skip this tick".
    pub(super) fn get_cached(self: &Arc<Self>) -> Option<u32> {
        let (value, needs_refresh) = {
            let guard = self.value.read();
            match *guard {
                Some(c) => {
                    let age = self.clock.now().saturating_duration_since(c.queried_at);
                    let stale = age.as_secs() >= ARCHIVE_CHECKPOINT_CACHE_SECS;
                    (Some(c.checkpoint), stale)
                }
                None => (None, true),
            }
        };

        if value.is_none() {
            self.cold_returns.fetch_add(1, Ordering::Relaxed);
        } else if needs_refresh {
            self.stale_returns.fetch_add(1, Ordering::Relaxed);
        }

        if needs_refresh {
            self.maybe_spawn_refresh();
        }

        value
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

    /// Overwrite the cached value. Used by `handle_catchup_result` to
    /// seed the cache with the ledger we just caught up to (an
    /// authoritative checkpoint value), and by tests to pre-warm the
    /// cache without going through the HTTP fetcher.
    pub(super) fn seed(&self, checkpoint: u32) {
        *self.value.write() = Some(CachedCheckpoint {
            checkpoint,
            queried_at: self.clock.now(),
        });
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

    /// Clear the cached value. Used by `trigger_recovery_catchup` to
    /// force the next non-blocking read to be "cold", so the background
    /// refresh spawns immediately — matching the previous behavior of
    /// invalidating the cache before a recovery-escalation archive query.
    pub(super) fn clear(&self) {
        *self.value.write() = None;
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

    pub(super) fn refresh_timeouts(&self) -> u64 {
        self.refresh_timeouts.load(Ordering::Relaxed)
    }

    pub(super) fn refresh_errors(&self) -> u64 {
        self.refresh_errors.load(Ordering::Relaxed)
    }

    pub(super) fn refresh_successes(&self) -> u64 {
        self.refresh_successes.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::Acquire)
    }

    fn maybe_spawn_refresh(self: &Arc<Self>) {
        // CAS: exactly one refresh runs at a time.
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::debug!("Archive checkpoint refresh already in flight; not spawning another");
            return;
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let fetcher = Arc::clone(&*this.background_fetcher.read());
            let fut = async { fetcher.fetch().await };
            let outcome =
                tokio::time::timeout(Duration::from_secs(ARCHIVE_REFRESH_TIMEOUT_SECS), fut).await;

            match outcome {
                Ok(Ok(checkpoint)) => {
                    *this.value.write() = Some(CachedCheckpoint {
                        checkpoint,
                        queried_at: this.clock.now(),
                    });
                    this.refresh_successes.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(checkpoint, "Archive checkpoint refresh succeeded");
                }
                Ok(Err(e)) => {
                    this.refresh_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        error = %e,
                        "Archive checkpoint refresh failed"
                    );
                }
                Err(_) => {
                    this.refresh_timeouts.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        timeout_secs = ARCHIVE_REFRESH_TIMEOUT_SECS,
                        "Archive checkpoint refresh timed out"
                    );
                }
            }

            this.refreshing.store(false, Ordering::Release);
        });
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
        assert_eq!(got, Some(999), "fresh cache returns seeded value");
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

    /// Cold cache returns `None` without blocking and triggers exactly one
    /// background refresh.
    #[tokio::test]
    async fn test_cold_cache_returns_none_and_spawns() {
        let gate = Arc::new(Notify::new());
        let (cache, fetcher) = mk_cache(MockResponse::BlockThenOk {
            gate: gate.clone(),
            checkpoint: 77,
        });

        let t0 = Instant::now();
        let got = cache.get_cached();
        let elapsed = t0.elapsed();
        assert_eq!(got, None, "cold cache returns None");
        assert!(
            elapsed < Duration::from_millis(50),
            "cold-cache get_cached must not block; took {:?}",
            elapsed
        );
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
        assert_eq!(got, Some(77));
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
            assert_eq!(h.await.unwrap(), None);
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
        assert_eq!(got, Some(100), "stale cache returns the stale value");
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
        assert_eq!(got, Some(200));
    }

    /// Refresh timeout fires, clears `refreshing`, and increments the
    /// timeout counter. Uses a hanging fetcher and `tokio::time::pause` so
    /// the test runs in ~0 ms of real time.
    #[tokio::test(start_paused = true)]
    async fn test_refresh_timeout_clears_refreshing() {
        let (cache, _fetcher) = mk_cache(MockResponse::Hang);

        let got = cache.get_cached();
        assert_eq!(got, None);
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
}
