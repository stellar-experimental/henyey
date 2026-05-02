//! Overlay network metrics collection.
//!
//! This module implements the OverlayMetrics from stellar-core, providing
//! comprehensive metrics for monitoring overlay network operations.
//!
//! # Overview
//!
//! Metrics are organized into categories:
//!
//! - **Message metrics**: Counts of messages read, written, dropped
//! - **Byte metrics**: Bytes read and written
//! - **Error metrics**: Read and write errors
//! - **Timeout metrics**: Idle and straggler timeouts
//! - **Connection metrics**: Connection latency and throttling
//! - **Receive timers**: Processing time per message type
//! - **Send counters**: Counts per message type sent
//! - **Queue metrics**: Outbound queue delays and drops
//! - **Flood metrics**: Transaction flooding statistics
//! - **Pull metrics**: Transaction pull latency
//!
//! # Thread Safety
//!
//! All metrics use atomic operations and are safe to access from multiple threads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Atomic counter for simple metrics.
#[derive(Debug, Default)]
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    /// Create a new counter starting at 0.
    pub fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// Increment the counter by 1.
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the counter by n.
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Set the counter to a specific value.
    pub fn set(&self, n: u64) {
        self.value.store(n, Ordering::Relaxed);
    }

    /// Get the current value.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Reset to 0 and return the previous value.
    pub fn reset(&self) -> u64 {
        self.value.swap(0, Ordering::Relaxed)
    }
}

/// Simple timer for tracking operation latencies.
#[derive(Debug)]
pub struct Timer {
    /// Total duration of all recorded operations.
    total_duration_us: AtomicU64,
    /// Number of operations recorded.
    count: AtomicU64,
    /// Minimum duration in microseconds.
    min_us: AtomicU64,
    /// Maximum duration in microseconds.
    max_us: AtomicU64,
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

impl Timer {
    /// Create a new timer.
    pub fn new() -> Self {
        Self {
            total_duration_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
        }
    }

    /// Record a duration.
    pub fn record(&self, duration: Duration) {
        let us = duration.as_micros() as u64;
        self.total_duration_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);

        // Update min (compare-and-swap loop)
        loop {
            let current_min = self.min_us.load(Ordering::Relaxed);
            if us >= current_min {
                break;
            }
            if self
                .min_us
                .compare_exchange_weak(current_min, us, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }

        // Update max
        loop {
            let current_max = self.max_us.load(Ordering::Relaxed);
            if us <= current_max {
                break;
            }
            if self
                .max_us
                .compare_exchange_weak(current_max, us, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Start timing an operation. Returns a guard that records the duration when dropped.
    pub fn start(&self) -> TimerGuard<'_> {
        TimerGuard {
            timer: self,
            start: Instant::now(),
        }
    }

    /// Get the number of recorded operations.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Get the total duration of all recorded operations.
    pub fn total_duration(&self) -> Duration {
        Duration::from_micros(self.total_duration_us.load(Ordering::Relaxed))
    }

    /// Get the average duration (returns 0 if no operations recorded).
    pub fn avg_duration(&self) -> Duration {
        let count = self.count();
        if count == 0 {
            return Duration::ZERO;
        }
        let total = self.total_duration_us.load(Ordering::Relaxed);
        Duration::from_micros(total / count)
    }

    /// Get the minimum duration (returns MAX if no operations recorded).
    pub fn min_duration(&self) -> Duration {
        let min = self.min_us.load(Ordering::Relaxed);
        if min == u64::MAX {
            Duration::ZERO
        } else {
            Duration::from_micros(min)
        }
    }

    /// Get the maximum duration.
    pub fn max_duration(&self) -> Duration {
        Duration::from_micros(self.max_us.load(Ordering::Relaxed))
    }

    /// Get a snapshot of timer statistics.
    pub fn snapshot(&self) -> TimerSnapshot {
        TimerSnapshot {
            count: self.count(),
            total: self.total_duration(),
            avg: self.avg_duration(),
            min: self.min_duration(),
            max: self.max_duration(),
        }
    }

    /// Reset all values.
    pub fn reset(&self) {
        self.total_duration_us.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
        self.min_us.store(u64::MAX, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
    }
}

/// Guard that records timer duration when dropped.
pub struct TimerGuard<'a> {
    timer: &'a Timer,
    start: Instant,
}

impl Drop for TimerGuard<'_> {
    fn drop(&mut self) {
        self.timer.record(self.start.elapsed());
    }
}

fn reset_counters(counters: &[&Counter]) {
    for counter in counters {
        counter.reset();
    }
}

fn reset_timers(timers: &[&Timer]) {
    for timer in timers {
        timer.reset();
    }
}

/// Snapshot of timer statistics.
#[derive(Debug, Clone)]
pub struct TimerSnapshot {
    /// Number of operations.
    pub count: u64,
    /// Total duration.
    pub total: Duration,
    /// Average duration.
    pub avg: Duration,
    /// Minimum duration.
    pub min: Duration,
    /// Maximum duration.
    pub max: Duration,
}

/// Overlay network metrics.
///
/// Provides comprehensive metrics for monitoring overlay operations.
/// All fields use atomic operations for thread safety.
#[derive(Debug, Default)]
pub struct OverlayMetrics {
    // ===== Message Metrics =====
    /// Messages read from peers.
    pub messages_read: Counter,
    /// Messages written to peers.
    pub messages_written: Counter,
    /// Messages dropped (queue full, etc).
    pub messages_dropped: Counter,
    /// Messages broadcast to all peers.
    pub messages_broadcast: Counter,

    // ===== Byte Metrics =====
    /// Bytes read from peers (wire-level: `AuthenticatedMessage` XDR body, excluding the
    /// 4-byte length header). Matches stellar-core `mByteRead`.
    pub bytes_read: Counter,
    /// Bytes written to peers (wire-level: `AuthenticatedMessage` XDR body, excluding
    /// the 4-byte length header). Matches stellar-core `mByteWrite`.
    pub bytes_written: Counter,

    // ===== Async I/O Metrics =====
    /// Successful recv I/O operations (each `Connection::recv*` returning a frame).
    /// Matches stellar-core `mAsyncRead`.
    pub async_read: Counter,
    /// Successful send I/O operations (each `Connection::send` returning Ok).
    /// Matches stellar-core `mAsyncWrite`.
    pub async_write: Counter,

    // ===== Connection Lifecycle Metrics =====
    /// Inbound connection accepts: `listener.accept()` returning `Ok`.
    /// Counts every TCP-accepted inbound connection, including those later rejected.
    pub inbound_attempt: Counter,
    /// Inbound connections that completed handshake and were fully registered as peers.
    /// Incremented exactly once per inbound peer that reaches `register_peer` Ok.
    pub inbound_establish: Counter,
    /// Inbound peer disconnections: incremented after `run_peer_loop` returns for an
    /// inbound peer (mirrors `inbound_establish`).
    pub inbound_drop: Counter,
    /// Inbound connections rejected at any point after accept but before establish
    /// (handshake failure, banned, duplicate, slots full, register race).
    pub inbound_reject: Counter,
    /// Outbound connection attempts: a dial was actually initiated (after the
    /// address reservation succeeded inside `connect_to_discovered_peer` /
    /// `connect_to_explicit_peer`). Does NOT include caller-side skips (e.g.,
    /// `add_peer` returning early because the pool is full before dialing) or
    /// in-flight-duplicate skips (where the address is already reserved by
    /// another in-progress dial) — those are not "attempts" in the wire sense.
    pub outbound_attempt: Counter,
    /// Outbound connections that completed handshake and were fully registered.
    pub outbound_establish: Counter,
    /// Outbound peer disconnections: incremented after `run_peer_loop` returns for an
    /// outbound peer (mirrors `outbound_establish`).
    pub outbound_drop: Counter,
    /// Outbound connections rejected at any point after attempt but before establish
    /// (TCP connect fail, handshake fail, banned, duplicate, slots full, register race).
    pub outbound_reject: Counter,

    // ===== Error Metrics =====
    /// Read errors encountered.
    pub errors_read: Counter,
    /// Write errors encountered.
    pub errors_write: Counter,

    // ===== Timeout Metrics =====
    /// Idle timeouts (peer not sending data).
    pub timeouts_idle: Counter,
    /// Straggler timeouts (peer too slow).
    pub timeouts_straggler: Counter,

    // ===== Connection Metrics =====
    /// Connection establishment latency.
    pub connection_latency: Timer,
    /// Time spent throttled on reads.
    pub connection_read_throttle: Timer,
    /// Time spent throttled on flood messages.
    pub connection_flood_throttle: Timer,
    /// Pending (unauthenticated) peer count.
    pub pending_peers: Counter,
    /// Authenticated peer count.
    pub authenticated_peers: Counter,

    // ===== Receive Timers =====
    /// Time to process Error messages.
    pub recv_error: Timer,
    /// Time to process Hello messages.
    pub recv_hello: Timer,
    /// Time to process Auth messages.
    pub recv_auth: Timer,
    /// Time to process DontHave messages.
    pub recv_dont_have: Timer,
    /// Time to process Peers messages.
    pub recv_peers: Timer,
    /// Time to process GetTxSet messages.
    pub recv_get_txset: Timer,
    /// Time to process TxSet messages.
    pub recv_txset: Timer,
    /// Time to process Transaction messages.
    pub recv_transaction: Timer,
    /// Time to process GetScpQuorumSet messages.
    pub recv_get_scp_qset: Timer,
    /// Time to process ScpQuorumSet messages.
    pub recv_scp_qset: Timer,
    /// Time to process ScpMessage messages.
    pub recv_scp_message: Timer,
    /// Time to process GetScpState messages.
    pub recv_get_scp_state: Timer,
    /// Time to process SendMore messages.
    pub recv_send_more: Timer,
    /// Time to process FloodAdvert messages.
    pub recv_flood_advert: Timer,
    /// Time to process FloodDemand messages.
    pub recv_flood_demand: Timer,
    /// Time to process SurveyRequest messages.
    pub recv_survey_request: Timer,
    /// Time to process SurveyResponse messages.
    pub recv_survey_response: Timer,

    // ===== Send Counters =====
    /// Error messages sent.
    pub send_error: Counter,
    /// Hello messages sent.
    pub send_hello: Counter,
    /// Auth messages sent.
    pub send_auth: Counter,
    /// DontHave messages sent.
    pub send_dont_have: Counter,
    /// Peers messages sent.
    pub send_peers: Counter,
    /// GetTxSet messages sent.
    pub send_get_txset: Counter,
    /// Transaction messages sent.
    pub send_transaction: Counter,
    /// TxSet messages sent.
    pub send_txset: Counter,
    /// GetScpQuorumSet messages sent.
    pub send_get_scp_qset: Counter,
    /// ScpQuorumSet messages sent.
    pub send_scp_qset: Counter,
    /// ScpMessage messages sent.
    pub send_scp_message: Counter,
    /// GetScpState messages sent.
    pub send_get_scp_state: Counter,
    /// SendMore messages sent.
    pub send_send_more: Counter,
    /// FloodAdvert messages sent.
    pub send_flood_advert: Counter,
    /// FloodDemand messages sent.
    pub send_flood_demand: Counter,
    /// SurveyRequest messages sent.
    pub send_survey_request: Counter,
    /// SurveyResponse messages sent.
    pub send_survey_response: Counter,

    // ===== Queue Metrics =====
    /// Queue delay for SCP messages.
    pub queue_delay_scp: Timer,
    /// Queue delay for transaction messages.
    pub queue_delay_tx: Timer,
    /// Queue delay for advert messages.
    pub queue_delay_advert: Timer,
    /// Queue delay for demand messages.
    pub queue_delay_demand: Timer,
    /// SCP messages dropped from queue.
    pub queue_drop_scp: Counter,
    /// Transaction messages dropped from queue.
    pub queue_drop_tx: Counter,
    /// Advert messages dropped from queue.
    pub queue_drop_advert: Counter,
    /// Demand messages dropped from queue.
    pub queue_drop_demand: Counter,

    // ===== Flood Metrics =====
    /// Messages demanded via FloodDemand.
    pub flood_demanded: Counter,
    /// Demands fulfilled (tx sent back).
    pub flood_fulfilled: Counter,
    /// Demands unfulfilled due to banned tx.
    pub flood_unfulfilled_banned: Counter,
    /// Demands unfulfilled due to unknown tx.
    pub flood_unfulfilled_unknown: Counter,
    /// Unique flood bytes received.
    pub flood_unique_bytes_recv: Counter,
    /// Duplicate flood bytes received.
    pub flood_duplicate_bytes_recv: Counter,
    /// Per-recipient flood deliveries (is_flood messages only).
    pub flood_broadcast: Counter,
    /// Unique flood messages received (inbound, record_seen → true).
    pub flood_unique_recv: Counter,
    /// Duplicate flood messages received (inbound, record_seen → false).
    pub flood_duplicate_recv: Counter,

    // ===== Fetch Metrics =====
    /// Unique fetch bytes received.
    pub fetch_unique_bytes_recv: Counter,
    /// Duplicate fetch bytes received.
    pub fetch_duplicate_bytes_recv: Counter,
    /// ItemFetcher next-peer selections (AskPeer results only).
    pub item_fetcher_next_peer: Counter,
    /// Unique/solicited fetch responses (TxSet/QSet tracked by ItemFetcher).
    pub fetch_unique_recv: Counter,
    /// Duplicate/unsolicited fetch responses (TxSet/QSet not tracked).
    pub fetch_duplicate_recv: Counter,

    // ===== Pull Metrics =====
    /// End-to-end transaction pull latency.
    pub tx_pull_latency: Timer,
    /// Per-peer transaction pull latency.
    pub peer_tx_pull_latency: Timer,
    /// Demand timeouts (retry needed).
    pub demand_timeouts: Counter,
    /// Pulled transactions that were relevant.
    pub pulled_relevant_txs: Counter,
    /// Pulled transactions that were irrelevant.
    pub pulled_irrelevant_txs: Counter,
    /// Abandoned demands (never received tx).
    pub abandoned_demands: Counter,
}

impl OverlayMetrics {
    /// Create a new metrics instance with all counters at 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of all metrics.
    pub fn snapshot(&self) -> OverlayMetricsSnapshot {
        OverlayMetricsSnapshot {
            // Message metrics
            messages_read: self.messages_read.get(),
            messages_written: self.messages_written.get(),
            messages_dropped: self.messages_dropped.get(),
            messages_broadcast: self.messages_broadcast.get(),

            // Byte metrics
            bytes_read: self.bytes_read.get(),
            bytes_written: self.bytes_written.get(),

            // Async I/O metrics
            async_read: self.async_read.get(),
            async_write: self.async_write.get(),

            // Connection lifecycle metrics
            inbound_attempt: self.inbound_attempt.get(),
            inbound_establish: self.inbound_establish.get(),
            inbound_drop: self.inbound_drop.get(),
            inbound_reject: self.inbound_reject.get(),
            outbound_attempt: self.outbound_attempt.get(),
            outbound_establish: self.outbound_establish.get(),
            outbound_drop: self.outbound_drop.get(),
            outbound_reject: self.outbound_reject.get(),

            // Error metrics
            errors_read: self.errors_read.get(),
            errors_write: self.errors_write.get(),

            // Timeout metrics
            timeouts_idle: self.timeouts_idle.get(),
            timeouts_straggler: self.timeouts_straggler.get(),

            // Connection metrics
            connection_latency: self.connection_latency.snapshot(),
            pending_peers: self.pending_peers.get(),
            authenticated_peers: self.authenticated_peers.get(),

            // Receive timers (just counts)
            recv_hello_count: self.recv_hello.count(),
            recv_auth_count: self.recv_auth.count(),
            recv_transaction_count: self.recv_transaction.count(),
            recv_scp_message_count: self.recv_scp_message.count(),
            recv_flood_advert_count: self.recv_flood_advert.count(),
            recv_flood_demand_count: self.recv_flood_demand.count(),

            // Send counters
            send_hello: self.send_hello.get(),
            send_auth: self.send_auth.get(),
            send_transaction: self.send_transaction.get(),
            send_scp_message: self.send_scp_message.get(),
            send_flood_advert: self.send_flood_advert.get(),
            send_flood_demand: self.send_flood_demand.get(),

            // Queue metrics
            queue_drop_scp: self.queue_drop_scp.get(),
            queue_drop_tx: self.queue_drop_tx.get(),
            queue_drop_advert: self.queue_drop_advert.get(),
            queue_drop_demand: self.queue_drop_demand.get(),

            // Flood metrics
            flood_demanded: self.flood_demanded.get(),
            flood_fulfilled: self.flood_fulfilled.get(),
            flood_unfulfilled_banned: self.flood_unfulfilled_banned.get(),
            flood_unfulfilled_unknown: self.flood_unfulfilled_unknown.get(),
            flood_unique_bytes_recv: self.flood_unique_bytes_recv.get(),
            flood_duplicate_bytes_recv: self.flood_duplicate_bytes_recv.get(),
            flood_broadcast: self.flood_broadcast.get(),
            flood_unique_recv: self.flood_unique_recv.get(),
            flood_duplicate_recv: self.flood_duplicate_recv.get(),

            // Fetch metrics
            fetch_unique_bytes_recv: self.fetch_unique_bytes_recv.get(),
            fetch_duplicate_bytes_recv: self.fetch_duplicate_bytes_recv.get(),
            item_fetcher_next_peer: self.item_fetcher_next_peer.get(),
            fetch_unique_recv: self.fetch_unique_recv.get(),
            fetch_duplicate_recv: self.fetch_duplicate_recv.get(),

            // Populated externally by the app layer from FloodGate::stats().
            flood_known_count: 0,

            // Pull metrics
            tx_pull_latency: self.tx_pull_latency.snapshot(),
            demand_timeouts: self.demand_timeouts.get(),
            pulled_relevant_txs: self.pulled_relevant_txs.get(),
            pulled_irrelevant_txs: self.pulled_irrelevant_txs.get(),
            abandoned_demands: self.abandoned_demands.get(),
        }
    }

    /// Reset all metrics to initial state.
    pub fn reset(&self) {
        reset_counters(&[
            &self.messages_read,
            &self.messages_written,
            &self.messages_dropped,
            &self.messages_broadcast,
            &self.bytes_read,
            &self.bytes_written,
            &self.async_read,
            &self.async_write,
            &self.inbound_attempt,
            &self.inbound_establish,
            &self.inbound_drop,
            &self.inbound_reject,
            &self.outbound_attempt,
            &self.outbound_establish,
            &self.outbound_drop,
            &self.outbound_reject,
            &self.errors_read,
            &self.errors_write,
            &self.timeouts_idle,
            &self.timeouts_straggler,
            &self.pending_peers,
            &self.authenticated_peers,
            &self.send_error,
            &self.send_hello,
            &self.send_auth,
            &self.send_dont_have,
            &self.send_peers,
            &self.send_get_txset,
            &self.send_transaction,
            &self.send_txset,
            &self.send_get_scp_qset,
            &self.send_scp_qset,
            &self.send_scp_message,
            &self.send_get_scp_state,
            &self.send_send_more,
            &self.send_flood_advert,
            &self.send_flood_demand,
            &self.send_survey_request,
            &self.send_survey_response,
            &self.queue_drop_scp,
            &self.queue_drop_tx,
            &self.queue_drop_advert,
            &self.queue_drop_demand,
            &self.flood_demanded,
            &self.flood_fulfilled,
            &self.flood_unfulfilled_banned,
            &self.flood_unfulfilled_unknown,
            &self.flood_unique_bytes_recv,
            &self.flood_duplicate_bytes_recv,
            &self.flood_broadcast,
            &self.flood_unique_recv,
            &self.flood_duplicate_recv,
            &self.fetch_unique_bytes_recv,
            &self.fetch_duplicate_bytes_recv,
            &self.item_fetcher_next_peer,
            &self.fetch_unique_recv,
            &self.fetch_duplicate_recv,
            &self.demand_timeouts,
            &self.pulled_relevant_txs,
            &self.pulled_irrelevant_txs,
            &self.abandoned_demands,
        ]);

        reset_timers(&[
            &self.connection_latency,
            &self.connection_read_throttle,
            &self.connection_flood_throttle,
            &self.recv_error,
            &self.recv_hello,
            &self.recv_auth,
            &self.recv_dont_have,
            &self.recv_peers,
            &self.recv_get_txset,
            &self.recv_txset,
            &self.recv_transaction,
            &self.recv_get_scp_qset,
            &self.recv_scp_qset,
            &self.recv_scp_message,
            &self.recv_get_scp_state,
            &self.recv_send_more,
            &self.recv_flood_advert,
            &self.recv_flood_demand,
            &self.recv_survey_request,
            &self.recv_survey_response,
            &self.queue_delay_scp,
            &self.queue_delay_tx,
            &self.queue_delay_advert,
            &self.queue_delay_demand,
            &self.tx_pull_latency,
            &self.peer_tx_pull_latency,
        ]);
    }
}

/// Snapshot of overlay metrics at a point in time.
#[derive(Debug, Clone)]
pub struct OverlayMetricsSnapshot {
    // Message metrics
    pub messages_read: u64,
    pub messages_written: u64,
    pub messages_dropped: u64,
    pub messages_broadcast: u64,

    // Byte metrics
    pub bytes_read: u64,
    pub bytes_written: u64,

    // Async I/O metrics
    pub async_read: u64,
    pub async_write: u64,

    // Connection lifecycle metrics
    pub inbound_attempt: u64,
    pub inbound_establish: u64,
    pub inbound_drop: u64,
    pub inbound_reject: u64,
    pub outbound_attempt: u64,
    pub outbound_establish: u64,
    pub outbound_drop: u64,
    pub outbound_reject: u64,

    // Error metrics
    pub errors_read: u64,
    pub errors_write: u64,

    // Timeout metrics
    pub timeouts_idle: u64,
    pub timeouts_straggler: u64,

    // Connection metrics
    pub connection_latency: TimerSnapshot,
    pub pending_peers: u64,
    pub authenticated_peers: u64,

    // Receive counts
    pub recv_hello_count: u64,
    pub recv_auth_count: u64,
    pub recv_transaction_count: u64,
    pub recv_scp_message_count: u64,
    pub recv_flood_advert_count: u64,
    pub recv_flood_demand_count: u64,

    // Send counts
    pub send_hello: u64,
    pub send_auth: u64,
    pub send_transaction: u64,
    pub send_scp_message: u64,
    pub send_flood_advert: u64,
    pub send_flood_demand: u64,

    // Queue drops
    pub queue_drop_scp: u64,
    pub queue_drop_tx: u64,
    pub queue_drop_advert: u64,
    pub queue_drop_demand: u64,

    // Flood metrics
    pub flood_demanded: u64,
    pub flood_fulfilled: u64,
    pub flood_unfulfilled_banned: u64,
    pub flood_unfulfilled_unknown: u64,
    pub flood_unique_bytes_recv: u64,
    pub flood_duplicate_bytes_recv: u64,
    pub flood_broadcast: u64,
    pub flood_unique_recv: u64,
    pub flood_duplicate_recv: u64,

    // Fetch metrics
    pub fetch_unique_bytes_recv: u64,
    pub fetch_duplicate_bytes_recv: u64,
    pub item_fetcher_next_peer: u64,
    pub fetch_unique_recv: u64,
    pub fetch_duplicate_recv: u64,

    /// FloodGate known entries (populated by the app layer, not by OverlayMetrics).
    pub flood_known_count: u64,

    // Pull metrics
    pub tx_pull_latency: TimerSnapshot,
    pub demand_timeouts: u64,
    pub pulled_relevant_txs: u64,
    pub pulled_irrelevant_txs: u64,
    pub abandoned_demands: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_counter_basic() {
        let counter = Counter::new();
        assert_eq!(counter.get(), 0);

        counter.inc();
        assert_eq!(counter.get(), 1);

        counter.add(5);
        assert_eq!(counter.get(), 6);

        counter.set(100);
        assert_eq!(counter.get(), 100);

        let prev = counter.reset();
        assert_eq!(prev, 100);
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn test_counter_concurrent() {
        let counter = Counter::new();
        let counter_ref = &counter;

        thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| {
                    for _ in 0..100 {
                        counter_ref.inc();
                    }
                });
            }
        });

        assert_eq!(counter.get(), 1000);
    }

    #[test]
    fn test_timer_basic() {
        let timer = Timer::new();
        assert_eq!(timer.count(), 0);

        timer.record(Duration::from_millis(10));
        timer.record(Duration::from_millis(20));
        timer.record(Duration::from_millis(30));

        assert_eq!(timer.count(), 3);

        let snapshot = timer.snapshot();
        assert_eq!(snapshot.count, 3);
        // Total should be around 60ms (60000 us)
        assert!(snapshot.total.as_micros() >= 59000);
        assert!(snapshot.total.as_micros() <= 61000);
        // Min should be around 10ms
        assert!(snapshot.min.as_millis() >= 9);
        assert!(snapshot.min.as_millis() <= 11);
        // Max should be around 30ms
        assert!(snapshot.max.as_millis() >= 29);
        assert!(snapshot.max.as_millis() <= 31);
    }

    #[test]
    fn test_timer_guard() {
        let timer = Timer::new();

        {
            let _guard = timer.start();
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(timer.count(), 1);
        assert!(timer.total_duration().as_millis() >= 4);
    }

    #[test]
    fn test_timer_concurrent() {
        let timer = Timer::new();
        let timer_ref = &timer;

        thread::scope(|s| {
            for _ in 0..10 {
                s.spawn(|| {
                    for _ in 0..10 {
                        timer_ref.record(Duration::from_micros(100));
                    }
                });
            }
        });

        assert_eq!(timer.count(), 100);
    }

    #[test]
    fn test_overlay_metrics_creation() {
        let metrics = OverlayMetrics::new();

        assert_eq!(metrics.messages_read.get(), 0);
        assert_eq!(metrics.bytes_written.get(), 0);
        assert_eq!(metrics.connection_latency.count(), 0);
    }

    #[test]
    fn test_overlay_metrics_increment() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.inc();
        metrics.messages_read.inc();
        metrics.bytes_read.add(1024);
        metrics.send_hello.inc();

        assert_eq!(metrics.messages_read.get(), 2);
        assert_eq!(metrics.bytes_read.get(), 1024);
        assert_eq!(metrics.send_hello.get(), 1);
    }

    #[test]
    fn test_overlay_metrics_snapshot() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.add(100);
        metrics.messages_written.add(50);
        metrics.bytes_read.add(10000);
        metrics.errors_read.inc();
        metrics.authenticated_peers.set(5);

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.messages_read, 100);
        assert_eq!(snapshot.messages_written, 50);
        assert_eq!(snapshot.bytes_read, 10000);
        assert_eq!(snapshot.errors_read, 1);
        assert_eq!(snapshot.authenticated_peers, 5);
    }

    #[test]
    fn test_overlay_metrics_reset() {
        let metrics = OverlayMetrics::new();

        metrics.messages_read.add(100);
        metrics.bytes_read.add(10000);
        metrics.connection_latency.record(Duration::from_millis(10));

        metrics.reset();

        assert_eq!(metrics.messages_read.get(), 0);
        assert_eq!(metrics.bytes_read.get(), 0);
        assert_eq!(metrics.connection_latency.count(), 0);
    }

    #[test]
    fn test_timer_empty_avg() {
        let timer = Timer::new();
        assert_eq!(timer.avg_duration(), Duration::ZERO);
        assert_eq!(timer.min_duration(), Duration::ZERO);
        assert_eq!(timer.max_duration(), Duration::ZERO);
    }

    #[test]
    fn test_flood_metrics() {
        let metrics = OverlayMetrics::new();

        // Simulate flood operations
        metrics.flood_demanded.add(10);
        metrics.flood_fulfilled.add(8);
        metrics.flood_unfulfilled_unknown.add(2);
        metrics.flood_unique_bytes_recv.add(50000);
        metrics.flood_duplicate_bytes_recv.add(10000);

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.flood_demanded, 10);
        assert_eq!(snapshot.flood_fulfilled, 8);
        assert_eq!(snapshot.flood_unfulfilled_unknown, 2);
        assert_eq!(snapshot.flood_unique_bytes_recv, 50000);
        assert_eq!(snapshot.flood_duplicate_bytes_recv, 10000);
    }

    #[test]
    fn test_pull_latency_metrics() {
        let metrics = OverlayMetrics::new();

        // Simulate pull latencies
        metrics.tx_pull_latency.record(Duration::from_millis(100));
        metrics.tx_pull_latency.record(Duration::from_millis(200));
        metrics.tx_pull_latency.record(Duration::from_millis(150));

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.tx_pull_latency.count, 3);
        assert!(snapshot.tx_pull_latency.min.as_millis() >= 99);
        assert!(snapshot.tx_pull_latency.max.as_millis() >= 199);
    }

    #[test]
    fn test_stage_f2_counters_in_snapshot() {
        let metrics = OverlayMetrics::new();

        metrics.flood_broadcast.add(5);
        metrics.flood_unique_recv.inc();
        metrics.flood_unique_recv.inc();
        metrics.flood_duplicate_recv.inc();
        metrics.fetch_unique_recv.add(3);
        metrics.fetch_duplicate_recv.add(7);
        metrics.item_fetcher_next_peer.add(4);

        let snap = metrics.snapshot();
        assert_eq!(snap.flood_broadcast, 5);
        assert_eq!(snap.flood_unique_recv, 2);
        assert_eq!(snap.flood_duplicate_recv, 1);
        assert_eq!(snap.fetch_unique_recv, 3);
        assert_eq!(snap.fetch_duplicate_recv, 7);
        assert_eq!(snap.item_fetcher_next_peer, 4);
    }

    #[test]
    fn test_stage_f2_counters_reset() {
        let metrics = OverlayMetrics::new();

        metrics.flood_broadcast.add(10);
        metrics.flood_unique_recv.add(20);
        metrics.flood_duplicate_recv.add(30);
        metrics.fetch_unique_recv.add(40);
        metrics.fetch_duplicate_recv.add(50);
        metrics.item_fetcher_next_peer.add(60);

        metrics.reset();

        let snap = metrics.snapshot();
        assert_eq!(snap.flood_broadcast, 0);
        assert_eq!(snap.flood_unique_recv, 0);
        assert_eq!(snap.flood_duplicate_recv, 0);
        assert_eq!(snap.fetch_unique_recv, 0);
        assert_eq!(snap.fetch_duplicate_recv, 0);
        assert_eq!(snap.item_fetcher_next_peer, 0);
    }
}
