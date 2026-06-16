//! In-process medida-compatible rate (EWMA meter) and percentile (R7 reservoir)
//! accumulators for the stellar-core compatibility `/metrics` endpoint.
//!
//! stellar-core's `/metrics` endpoint reports medida-format JSON in which
//! `meter` and `timer` metrics carry exponentially-weighted moving-average
//! (EWMA) rates (`1_min_rate`/`5_min_rate`/`15_min_rate`/`mean_rate`) and
//! `timer`/`histogram` metrics carry duration percentiles
//! (`median`/`75%`/`95%`/`98%`/`99%`/`99.9%`/`100%`) plus
//! `min`/`max`/`mean`/`stddev`/`sum`. henyey's native metrics layer is built on
//! Prometheus primitives and does NOT maintain these in-process, so the compat
//! handler previously emitted hardcoded `0.0`s for every rate/percentile field.
//!
//! This module fills that gap for exactly the four metrics SSC missions read:
//! `ledger.ledger.close` (timer), `ledger.transaction.count` (histogram),
//! `scp.value.valid` and `scp.value.invalid` (meters). No other metric is
//! instrumented here.
//!
//! ## Parity
//!
//! - [`EwmaMeter`] is a faithful port of stellar-core's
//!   `lib/libmedida/src/medida/stats/ewma.cc` + `meter.cc`: the alpha constants
//!   `1 - exp(-5 / (60 * N))` for N ∈ {1, 5, 15} minutes, the 5-second LAZY
//!   `TickIfNecessary` model (ticks are applied on read/mark, NOT via an
//!   event-loop timer), first-sample seeding, and `mean_rate = count * 1e9 /
//!   elapsed_ns`. The clock is injectable so the decay behaviour is
//!   deterministically testable. This is a high-fidelity port.
//!
//! - [`ReservoirSample`] is a **documented approximation** of stellar-core's
//!   timer/histogram percentiles. stellar-core's default `Timer`/`Histogram`
//!   sample is a **CKMS** error-bounded streaming sketch over a 30-second
//!   sliding window (`Timer::GetSnapshot()` → `CKMSImpl::getValue`). This module
//!   instead keeps the last 256 observations in a fixed-capacity ring and
//!   computes percentiles with the **R7 (Hyndman-Fan) interpolation over a
//!   sorted vector** copied verbatim from medida's `snapshot.cc
//!   Snapshot::VectorImpl::getValue`. The divergences are (a) algorithm — R7
//!   exact-sorted interpolation vs CKMS error-bounded sketch — and (b) window —
//!   a 256-observation capacity window vs CKMS's 30-second time window. This is
//!   sufficient for SSC's presence/ordering/non-zero assertion class (SSC has no
//!   oracle for the exact CKMS values a henyey node would produce). See
//!   `crates/app/PARITY_STATUS.md`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Tick interval: stellar-core ticks the EWMAs every 5 seconds.
const TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Fixed reservoir capacity (capacity-windowed, NOT time-windowed — see module docs).
const RESERVOIR_CAPACITY: usize = 256;

// EWMA alpha constants — `alpha = 1 - exp(-INTERVAL / SECONDS_PER_MINUTE / N)`,
// matching `medida/stats/ewma.cc`'s kM1/kM5/kM15_ALPHA. INTERVAL = 5s.
fn m1_alpha() -> f64 {
    1.0 - (-5.0_f64 / 60.0 / 1.0).exp()
}
fn m5_alpha() -> f64 {
    1.0 - (-5.0_f64 / 60.0 / 5.0).exp()
}
fn m15_alpha() -> f64 {
    1.0 - (-5.0_f64 / 60.0 / 15.0).exp()
}

/// A single exponentially-weighted moving average — port of medida's
/// `stats::EWMA::Impl`.
///
/// `rate_` is stored in events-per-nanosecond (medida uses `count /
/// interval_nanos`); [`Ewma::rate`] scales it to events-per-second.
#[derive(Debug, Clone)]
struct Ewma {
    initialized: bool,
    /// Rate in events per nanosecond.
    rate: f64,
    /// Accumulated count not yet folded into `rate` by a tick.
    uncounted: i64,
    alpha: f64,
    interval_nanos: f64,
}

impl Ewma {
    fn new(alpha: f64) -> Self {
        Ewma {
            initialized: false,
            rate: 0.0,
            uncounted: 0,
            alpha,
            interval_nanos: TICK_INTERVAL.as_nanos() as f64,
        }
    }

    /// `medida EWMA::Impl::update` — accumulate uncounted events.
    fn update(&mut self, n: i64) {
        self.uncounted += n;
    }

    /// `medida EWMA::Impl::tick` — fold the uncounted events into the rate.
    fn tick(&mut self) {
        let count = self.uncounted as f64;
        self.uncounted = 0;
        let instant_rate = count / self.interval_nanos;
        if self.initialized {
            self.rate += self.alpha * (instant_rate - self.rate);
        } else {
            self.rate = instant_rate;
            self.initialized = true;
        }
    }

    /// `medida EWMA::Impl::getRate` — scale to events per `duration`. Default
    /// duration is one second, matching medida's `getRate()`.
    fn rate(&self, duration: Duration) -> f64 {
        self.rate * duration.as_nanos() as f64
    }
}

/// A medida-compatible meter — port of `medida::Meter::Impl`.
///
/// Holds three EWMAs (1/5/15-minute), a cumulative count, and a start instant.
/// Ticks are applied lazily on `mark`/read based on the injectable clock, so no
/// event-loop timer is required.
#[derive(Debug)]
struct MeterInner {
    count: u64,
    start: Instant,
    last_tick: Instant,
    m1: Ewma,
    m5: Ewma,
    m15: Ewma,
}

impl MeterInner {
    fn new(now: Instant) -> Self {
        MeterInner {
            count: 0,
            start: now,
            last_tick: now,
            m1: Ewma::new(m1_alpha()),
            m5: Ewma::new(m5_alpha()),
            m15: Ewma::new(m15_alpha()),
        }
    }

    /// `medida Meter::Impl::TickIfNecessary` — apply `age / 5s` ticks.
    fn tick_if_necessary(&mut self, now: Instant) {
        // `now` may be earlier than `last_tick` only under clock skew; saturate.
        let age = now.saturating_duration_since(self.last_tick);
        if age > TICK_INTERVAL {
            let required_ticks = (age.as_nanos() / TICK_INTERVAL.as_nanos()) as u64;
            // Advance last_tick by exactly the number of whole intervals consumed,
            // mirroring stellar-core which sets last_tick = now (it recomputes age
            // from the same `now`); we advance by whole intervals so a fractional
            // remainder is carried to the next call (equivalent for whole counts).
            self.last_tick += TICK_INTERVAL * (required_ticks as u32);
            for _ in 0..required_ticks {
                self.m1.tick();
                self.m5.tick();
                self.m15.tick();
            }
        }
    }

    /// `medida Meter::Impl::Mark` — tick-if-necessary THEN add.
    fn mark(&mut self, n: u64, now: Instant) {
        self.tick_if_necessary(now);
        self.count += n;
        self.m1.update(n as i64);
        self.m5.update(n as i64);
        self.m15.update(n as i64);
    }

    fn one_minute_rate(&mut self, now: Instant) -> f64 {
        self.tick_if_necessary(now);
        self.m1.rate(Duration::from_secs(1))
    }
    fn five_minute_rate(&mut self, now: Instant) -> f64 {
        self.tick_if_necessary(now);
        self.m5.rate(Duration::from_secs(1))
    }
    fn fifteen_minute_rate(&mut self, now: Instant) -> f64 {
        self.tick_if_necessary(now);
        self.m15.rate(Duration::from_secs(1))
    }

    /// `medida Meter::Impl::mean_rate` — `count * rate_unit_ns / elapsed_ns`.
    fn mean_rate(&self, now: Instant) -> f64 {
        if self.count > 0 {
            let elapsed = now.saturating_duration_since(self.start).as_nanos();
            if elapsed == 0 {
                return 0.0;
            }
            (self.count as f64) * 1e9 / (elapsed as f64)
        } else {
            0.0
        }
    }
}

/// A snapshot of a meter's rate fields, in events per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterSnapshot {
    pub count: u64,
    pub mean_rate: f64,
    pub one_minute_rate: f64,
    pub five_minute_rate: f64,
    pub fifteen_minute_rate: f64,
}

/// A medida-compatible meter with an injectable clock.
///
/// `mark` is called from the producing thread (e.g. the metrics refresh path);
/// reads happen from the async `/metrics` handler. All access is guarded by a
/// `Mutex` exactly as medida guards its `Meter::Impl`. Contention is negligible
/// — a few marks every ~5 seconds.
#[derive(Debug)]
pub struct EwmaMeter {
    inner: Mutex<MeterInner>,
}

impl EwmaMeter {
    /// Construct a meter whose start/last-tick instants are `now`.
    pub fn new_at(now: Instant) -> Self {
        EwmaMeter {
            inner: Mutex::new(MeterInner::new(now)),
        }
    }

    /// Construct a meter anchored at the current wall clock.
    pub fn new() -> Self {
        Self::new_at(Instant::now())
    }

    /// Mark `n` events at instant `now`.
    pub fn mark_at(&self, n: u64, now: Instant) {
        if n == 0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.mark(n, now);
    }

    /// Mark `n` events at the current wall clock.
    pub fn mark(&self, n: u64) {
        self.mark_at(n, Instant::now());
    }

    /// Snapshot all rate fields at instant `now`.
    pub fn snapshot_at(&self, now: Instant) -> MeterSnapshot {
        let mut inner = self.inner.lock().unwrap();
        MeterSnapshot {
            count: inner.count,
            mean_rate: inner.mean_rate(now),
            one_minute_rate: inner.one_minute_rate(now),
            five_minute_rate: inner.five_minute_rate(now),
            fifteen_minute_rate: inner.fifteen_minute_rate(now),
        }
    }

    /// Snapshot all rate fields at the current wall clock.
    pub fn snapshot(&self) -> MeterSnapshot {
        self.snapshot_at(Instant::now())
    }
}

impl Default for EwmaMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// R7 percentile interpolation over a sorted slice — verbatim port of medida's
/// `Snapshot::VectorImpl::getValue` (`snapshot.cc`).
///
/// `values` MUST be sorted ascending. `quantile` is in `[0.0, 1.0]`. Returns
/// `0.0` for an empty slice (matching medida).
fn r7_quantile(values: &[f64], quantile: f64) -> f64 {
    debug_assert!((0.0..=1.0).contains(&quantile));
    if values.is_empty() {
        return 0.0;
    }
    // Step 1: range of allowed indexes [0, max_idx].
    let max_idx = values.len() - 1;
    // Step 2: ideal fractional index (1.0 => max_idx).
    let ideal_index = quantile * (max_idx as f64);
    // Step 3: floor and integral lo/hi indexes.
    let floor_ideal = ideal_index.floor();
    let lo_idx = floor_ideal as usize;
    let hi_idx = lo_idx + 1;
    // Step 4: no upper sample to interpolate with => return the highest.
    if hi_idx > max_idx {
        return values[max_idx];
    }
    // Step 5: linear interpolation between lo and hi.
    let delta = ideal_index - floor_ideal;
    let lower = values[lo_idx];
    let upper = values[hi_idx];
    lower + delta * (upper - lower)
}

/// A snapshot of a reservoir's distribution statistics, in the reservoir's
/// stored unit (milliseconds, for the close timer / tx-count histogram).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReservoirSnapshot {
    pub count: u64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub sum: f64,
    pub median: f64,
    pub p75: f64,
    pub p95: f64,
    pub p98: f64,
    pub p99: f64,
    pub p999: f64,
}

impl ReservoirSnapshot {
    /// The all-zero snapshot returned for an empty reservoir.
    pub const ZERO: ReservoirSnapshot = ReservoirSnapshot {
        count: 0,
        min: 0.0,
        max: 0.0,
        mean: 0.0,
        stddev: 0.0,
        sum: 0.0,
        median: 0.0,
        p75: 0.0,
        p95: 0.0,
        p98: 0.0,
        p99: 0.0,
        p999: 0.0,
    };
}

/// A fixed-capacity ring of the last [`RESERVOIR_CAPACITY`] observations, with
/// R7 percentile interpolation — a documented approximation of medida's
/// CKMS-backed timer/histogram (see module docs).
#[derive(Debug)]
struct ReservoirInner {
    /// Ring buffer of observations (in the stored unit, ms).
    ring: Vec<f64>,
    /// Next write position.
    pos: usize,
    /// Total observations ever recorded (NOT the ring length).
    count: u64,
}

impl ReservoirInner {
    fn new() -> Self {
        ReservoirInner {
            ring: Vec::with_capacity(RESERVOIR_CAPACITY),
            pos: 0,
            count: 0,
        }
    }

    fn update(&mut self, value: f64) {
        if self.ring.len() < RESERVOIR_CAPACITY {
            self.ring.push(value);
        } else {
            self.ring[self.pos] = value;
            self.pos = (self.pos + 1) % RESERVOIR_CAPACITY;
        }
        self.count += 1;
    }

    fn snapshot(&self) -> ReservoirSnapshot {
        if self.ring.is_empty() {
            return ReservoirSnapshot::ZERO;
        }
        let n = self.ring.len();
        let mut sorted = self.ring.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let sum: f64 = sorted.iter().sum();
        let mean = sum / n as f64;
        // Sample stddev (n-1 denominator) matches medida's UniformSample/Histogram
        // stddev which divides by (count - 1); for n == 1 medida returns 0.0.
        let stddev = if n > 1 {
            let var = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
            var.sqrt()
        } else {
            0.0
        };

        ReservoirSnapshot {
            count: self.count,
            min: sorted[0],
            max: sorted[n - 1],
            mean,
            stddev,
            sum,
            median: r7_quantile(&sorted, 0.5),
            p75: r7_quantile(&sorted, 0.75),
            p95: r7_quantile(&sorted, 0.95),
            p98: r7_quantile(&sorted, 0.98),
            p99: r7_quantile(&sorted, 0.99),
            p999: r7_quantile(&sorted, 0.999),
        }
    }
}

/// A bounded-reservoir sample feeding R7 percentiles plus min/max/mean/stddev/sum,
/// used for the close timer and tx-count histogram. Thread-safe via a `Mutex`;
/// observations come from the ledger-close path (a few per ~5s), reads from the
/// async `/metrics` handler. See module docs for the CKMS divergence.
#[derive(Debug)]
pub struct ReservoirSample {
    inner: Mutex<ReservoirInner>,
}

impl ReservoirSample {
    pub fn new() -> Self {
        ReservoirSample {
            inner: Mutex::new(ReservoirInner::new()),
        }
    }

    /// Record an observation (in the stored unit, ms for our timers/histograms).
    pub fn update(&self, value: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.update(value);
    }

    /// Snapshot the current distribution statistics.
    pub fn snapshot(&self) -> ReservoirSnapshot {
        self.inner.lock().unwrap().snapshot()
    }
}

impl Default for ReservoirSample {
    fn default() -> Self {
        Self::new()
    }
}

/// The fixed, scoped set of medida-compat accumulators.
///
/// Exactly the four metrics SSC missions read. The close timer additionally
/// carries an embedded meter (for its rate fields, `event_type = "calls"`); the
/// scp meters use `event_type = "value"`.
#[derive(Debug)]
pub struct MedidaCompat {
    /// `ledger.ledger.close` percentiles/min/max/etc. (ms reservoir).
    pub close_timer: ReservoirSample,
    /// `ledger.ledger.close` rate fields (embedded meter, event_type "calls").
    pub close_meter: EwmaMeter,
    /// `ledger.transaction.count` percentiles/min/max/etc.
    pub tx_count_histogram: ReservoirSample,
    /// `scp.value.valid` meter (event_type "value").
    pub scp_value_valid: EwmaMeter,
    /// `scp.value.invalid` meter (event_type "value").
    pub scp_value_invalid: EwmaMeter,
    /// Last-seen cumulative scp counters, for delta-feeding the meters from the
    /// already-exposed `ScpMetricsSnapshot` without double-counting (the marks
    /// must be persisted across refresh ticks).
    pub scp_last_seen: Mutex<ScpLastSeen>,
}

/// Last-seen cumulative scp value counters used to compute per-tick deltas.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScpLastSeen {
    pub value_valid_total: u64,
    pub value_invalid_total: u64,
}

impl MedidaCompat {
    fn new() -> Self {
        MedidaCompat {
            close_timer: ReservoirSample::new(),
            close_meter: EwmaMeter::new(),
            tx_count_histogram: ReservoirSample::new(),
            scp_value_valid: EwmaMeter::new(),
            scp_value_invalid: EwmaMeter::new(),
            scp_last_seen: Mutex::new(ScpLastSeen::default()),
        }
    }

    /// Construct a fresh, isolated registry for unit tests (the process-global
    /// [`medida_compat`] singleton is shared and not suitable for assertions on
    /// exact values).
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::new()
    }

    /// Like [`new_for_test`], but anchors the meters' start/last-tick instants at
    /// `now`. Marking at `now` and reading the snapshot at a wall clock ≥5s later
    /// then exercises the lazy-tick rate path deterministically.
    #[cfg(test)]
    pub(crate) fn new_for_test_at(now: Instant) -> Self {
        MedidaCompat {
            close_timer: ReservoirSample::new(),
            close_meter: EwmaMeter::new_at(now),
            tx_count_histogram: ReservoirSample::new(),
            scp_value_valid: EwmaMeter::new_at(now),
            scp_value_invalid: EwmaMeter::new_at(now),
            scp_last_seen: Mutex::new(ScpLastSeen::default()),
        }
    }

    /// Record a ledger close: `close_ms` into the close timer + meter, `tx_count`
    /// into the tx-count histogram. Called from the ledger-close path.
    pub fn record_ledger_close(&self, close_ms: f64, tx_count: u64) {
        self.close_timer.update(close_ms);
        self.close_meter.mark(1);
        self.tx_count_histogram.update(tx_count as f64);
    }

    /// Like [`record_ledger_close`], but marks the close meter at an injected
    /// instant so the EWMA lazy-tick behaviour is deterministically testable.
    #[cfg(test)]
    pub(crate) fn record_ledger_close_at(&self, close_ms: f64, tx_count: u64, now: Instant) {
        self.close_timer.update(close_ms);
        self.close_meter.mark_at(1, now);
        self.tx_count_histogram.update(tx_count as f64);
    }

    /// Feed the scp meters from a cumulative `ScpMetricsSnapshot` by marking the
    /// delta since the last call. Called from the periodic metrics refresh so
    /// the marks are spread over time (truer EWMA shape than dumping the whole
    /// backlog at scrape). The cumulative totals are the meters' `count`.
    pub fn feed_scp(&self, value_valid_total: u64, value_invalid_total: u64) {
        let mut last = self.scp_last_seen.lock().unwrap();
        let valid_delta = value_valid_total.saturating_sub(last.value_valid_total);
        let invalid_delta = value_invalid_total.saturating_sub(last.value_invalid_total);
        last.value_valid_total = value_valid_total;
        last.value_invalid_total = value_invalid_total;
        drop(last);
        if valid_delta > 0 {
            self.scp_value_valid.mark(valid_delta);
        }
        if invalid_delta > 0 {
            self.scp_value_invalid.mark(invalid_delta);
        }
    }

    /// Like [`feed_scp`], but marks at an injected instant for deterministic
    /// rate tests.
    #[cfg(test)]
    pub(crate) fn feed_scp_at(
        &self,
        value_valid_total: u64,
        value_invalid_total: u64,
        now: Instant,
    ) {
        let mut last = self.scp_last_seen.lock().unwrap();
        let valid_delta = value_valid_total.saturating_sub(last.value_valid_total);
        let invalid_delta = value_invalid_total.saturating_sub(last.value_invalid_total);
        last.value_valid_total = value_valid_total;
        last.value_invalid_total = value_invalid_total;
        drop(last);
        if valid_delta > 0 {
            self.scp_value_valid.mark_at(valid_delta, now);
        }
        if invalid_delta > 0 {
            self.scp_value_invalid.mark_at(invalid_delta, now);
        }
    }
}

static MEDIDA_COMPAT: OnceLock<MedidaCompat> = OnceLock::new();

/// The process-global medida-compat registry.
pub fn medida_compat() -> &'static MedidaCompat {
    MEDIDA_COMPAT.get_or_init(MedidaCompat::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advance the EWMA by one simulated minute: 12 ticks of 5s each, with no
    /// new marks — mirrors medida's `elapseMinute` test helper.
    fn elapse_minute(meter: &EwmaMeter, base: Instant, minute_index: u64) -> Instant {
        // Advance the injected clock by 12 * 5s = 60s in 5s steps, reading the
        // rate each step so tick_if_necessary applies exactly one tick per read.
        let mut now = base;
        for step in 1..=12u64 {
            now = base + TICK_INTERVAL * ((minute_index * 12 + step) as u32);
            // A zero-mark snapshot applies the pending ticks without adding events.
            meter.snapshot_at(now);
        }
        now
    }

    /// The three EWMA alpha constants match medida's kM1/kM5/kM15_ALPHA.
    #[test]
    fn test_ewma_matches_medida_constants() {
        // medida: kM1_ALPHA = 1 - exp(-5/60/1), etc.
        assert!((m1_alpha() - 0.07995558_f64).abs() < 1e-7, "m1 alpha");
        assert!((m5_alpha() - 0.01652854_f64).abs() < 1e-7, "m5 alpha");
        assert!((m15_alpha() - 0.00554014_f64).abs() < 1e-7, "m15 alpha");
    }

    /// Port of medida `test_ewma.cc aOneMinuteEWMAWithAValueOfThree`: after one
    /// update(3)+tick the rate is 0.6/s, decaying over successive minutes.
    /// Drives a raw `Ewma` exactly as medida's EWMA unit test does.
    #[test]
    fn test_ewma_one_minute_decay_vector() {
        let mut ewma = Ewma::new(m1_alpha());
        ewma.update(3);
        ewma.tick();
        let r = ewma.rate(Duration::from_secs(1));
        assert!((r - 0.6).abs() < 1e-6, "initial rate {r}");
        // Expected values from medida test_ewma.cc after each elapsed minute.
        let expected = [
            0.22072766, 0.08120117, 0.02987224, 0.01098938, 0.00404277, 0.00148725, 0.00054713,
            0.00020128, 0.00007405, 0.00002724, 0.00001002, 0.00000369, 0.00000136, 0.00000050,
            0.00000018,
        ];
        for e in expected {
            for _ in 0..12 {
                ewma.tick();
            }
            let r = ewma.rate(Duration::from_secs(1));
            assert!((r - e).abs() < 1e-6, "expected {e}, got {r}");
        }
    }

    /// Port of medida `test_ewma.cc aFifteenMinuteEWMAWithAValueOfThree` first
    /// few steps, validating the 15-minute alpha decay.
    #[test]
    fn test_ewma_fifteen_minute_decay_vector() {
        let mut ewma = Ewma::new(m15_alpha());
        ewma.update(3);
        ewma.tick();
        assert!((ewma.rate(Duration::from_secs(1)) - 0.6).abs() < 1e-6);
        let expected = [0.56130419, 0.52510399, 0.49123845, 0.45955700];
        for e in expected {
            for _ in 0..12 {
                ewma.tick();
            }
            assert!((ewma.rate(Duration::from_secs(1)) - e).abs() < 1e-6);
        }
    }

    /// The lazy tick advances the rate on elapsed wall-clock WITHOUT new marks:
    /// mark once, then advance the injected clock and assert the 1-min rate
    /// decays toward zero. Validates the injectable-clock decay path.
    #[test]
    fn test_ewma_decays_with_injected_clock() {
        let base = Instant::now();
        let meter = EwmaMeter::new_at(base);
        // Mark 3 events, then apply the first tick by reading 5s later.
        meter.mark_at(3, base);
        let after_first_tick = base + TICK_INTERVAL + Duration::from_millis(1);
        let snap0 = meter.snapshot_at(after_first_tick);
        // After one tick: instant rate = 3 / 5s = 0.6/s.
        assert!(
            (snap0.one_minute_rate - 0.6).abs() < 1e-6,
            "first-tick rate {}",
            snap0.one_minute_rate
        );
        // Advance one minute with no new marks — rate must decay.
        let now = elapse_minute(&meter, after_first_tick, 0);
        let snap1 = meter.snapshot_at(now);
        assert!(
            snap1.one_minute_rate < snap0.one_minute_rate,
            "rate should decay: {} !< {}",
            snap1.one_minute_rate,
            snap0.one_minute_rate
        );
        assert!(snap1.one_minute_rate > 0.0, "rate still positive");
        // After many minutes it approaches zero.
        let mut t = now;
        for m in 1..=10 {
            t = elapse_minute(&meter, after_first_tick, m);
        }
        let snap_final = meter.snapshot_at(t);
        assert!(
            snap_final.one_minute_rate < 0.01,
            "rate decayed near zero: {}",
            snap_final.one_minute_rate
        );
    }

    /// A fresh meter with no marks reports 0.0 for all rates and mean_rate.
    #[test]
    fn test_ewma_zero_at_startup() {
        let base = Instant::now();
        let meter = EwmaMeter::new_at(base);
        let snap = meter.snapshot_at(base + Duration::from_secs(30));
        assert_eq!(snap.count, 0);
        assert_eq!(snap.mean_rate, 0.0);
        assert_eq!(snap.one_minute_rate, 0.0);
        assert_eq!(snap.five_minute_rate, 0.0);
        assert_eq!(snap.fifteen_minute_rate, 0.0);
    }

    /// mean_rate = count * 1e9 / elapsed_ns. Two marks over 10s ⇒ 0.2/s.
    #[test]
    fn test_ewma_mean_rate() {
        let base = Instant::now();
        let meter = EwmaMeter::new_at(base);
        meter.mark_at(1, base);
        meter.mark_at(1, base);
        let snap = meter.snapshot_at(base + Duration::from_secs(10));
        assert_eq!(snap.count, 2);
        assert!(
            (snap.mean_rate - 0.2).abs() < 1e-9,
            "mean_rate {}",
            snap.mean_rate
        );
    }

    /// R7 percentiles over a known sample match the medida VectorImpl formula,
    /// and are monotonically ordered.
    #[test]
    fn test_reservoir_r7_percentiles() {
        let res = ReservoirSample::new();
        // 1..=100 inclusive.
        for i in 1..=100u64 {
            res.update(i as f64);
        }
        let snap = res.snapshot();
        assert_eq!(snap.count, 100);
        assert_eq!(snap.min, 1.0);
        assert_eq!(snap.max, 100.0);
        assert!((snap.mean - 50.5).abs() < 1e-9, "mean {}", snap.mean);
        // R7 over sorted [1..100], max_idx=99:
        //   median: ideal=0.5*99=49.5 => 50 + 0.5*(51-50)=50.5
        //   p75:    ideal=0.75*99=74.25 => 75 + 0.25*(76-75)=75.25
        //   p95:    ideal=0.95*99=94.05 => 95 + 0.05*(96-95)=95.05
        //   p99:    ideal=0.99*99=98.01 => 99 + 0.01*(100-99)=99.01
        assert!((snap.median - 50.5).abs() < 1e-9, "median {}", snap.median);
        assert!((snap.p75 - 75.25).abs() < 1e-9, "p75 {}", snap.p75);
        assert!((snap.p95 - 95.05).abs() < 1e-9, "p95 {}", snap.p95);
        assert!((snap.p99 - 99.01).abs() < 1e-9, "p99 {}", snap.p99);
        // Ordering.
        assert!(snap.median <= snap.p75);
        assert!(snap.p75 <= snap.p95);
        assert!(snap.p95 <= snap.p99);
        assert!(snap.p99 <= snap.p999);
        assert!(snap.p999 <= snap.max);
    }

    /// Direct R7 unit check against medida's getValue on a tiny vector.
    #[test]
    fn test_r7_quantile_interpolation() {
        // sorted [10, 20, 30, 40], max_idx=3.
        let v = [10.0, 20.0, 30.0, 40.0];
        // median: ideal=0.5*3=1.5 => 20 + 0.5*(30-20)=25.
        assert!((r7_quantile(&v, 0.5) - 25.0).abs() < 1e-9);
        // p75: ideal=0.75*3=2.25 => 30 + 0.25*(40-30)=32.5.
        assert!((r7_quantile(&v, 0.75) - 32.5).abs() < 1e-9);
        // q=1.0 => max.
        assert!((r7_quantile(&v, 1.0) - 40.0).abs() < 1e-9);
        // q=0.0 => min.
        assert!((r7_quantile(&v, 0.0) - 10.0).abs() < 1e-9);
    }

    /// An empty reservoir reports all-zero percentiles/min/max/mean/stddev.
    #[test]
    fn test_reservoir_empty_is_zero() {
        let res = ReservoirSample::new();
        let snap = res.snapshot();
        assert_eq!(snap, ReservoirSnapshot::ZERO);
        assert_eq!(snap.count, 0);
        assert_eq!(snap.median, 0.0);
        assert_eq!(snap.p99, 0.0);
        assert_eq!(snap.min, 0.0);
        assert_eq!(snap.max, 0.0);
    }

    /// The ring keeps only the last RESERVOIR_CAPACITY observations.
    #[test]
    fn test_reservoir_ring_evicts() {
        let res = ReservoirSample::new();
        // Insert 256 small values, then 256 large values — the large ones evict
        // the small ones, so min/max reflect only the large window.
        for _ in 0..RESERVOIR_CAPACITY {
            res.update(1.0);
        }
        for _ in 0..RESERVOIR_CAPACITY {
            res.update(1000.0);
        }
        let snap = res.snapshot();
        assert_eq!(snap.count, (RESERVOIR_CAPACITY * 2) as u64);
        assert_eq!(snap.min, 1000.0, "small values should be evicted");
        assert_eq!(snap.max, 1000.0);
        assert!((snap.mean - 1000.0).abs() < 1e-9);
    }

    /// stddev uses the (n-1) sample denominator and is 0.0 for a single sample.
    #[test]
    fn test_reservoir_stddev() {
        let res = ReservoirSample::new();
        res.update(5.0);
        assert_eq!(res.snapshot().stddev, 0.0, "single sample stddev is 0");
        let res2 = ReservoirSample::new();
        for v in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            res2.update(v);
        }
        // Sample stddev of this classic dataset (n-1 denom) is ~2.13809.
        let s = res2.snapshot().stddev;
        assert!((s - 2.1380899).abs() < 1e-6, "stddev {s}");
    }

    /// Delta-feeding the scp meters from cumulative snapshots marks only the
    /// delta and tracks the cumulative count.
    #[test]
    fn test_feed_scp_delta() {
        let compat = MedidaCompat::new();
        compat.feed_scp(10, 2);
        assert_eq!(compat.scp_value_valid.snapshot().count, 10);
        assert_eq!(compat.scp_value_invalid.snapshot().count, 2);
        // Second feed marks only the delta (15-10=5, 3-2=1).
        compat.feed_scp(15, 3);
        assert_eq!(compat.scp_value_valid.snapshot().count, 15);
        assert_eq!(compat.scp_value_invalid.snapshot().count, 3);
        // Non-increasing totals (restart) mark nothing.
        compat.feed_scp(15, 3);
        assert_eq!(compat.scp_value_valid.snapshot().count, 15);
    }

    /// record_ledger_close feeds both the close timer/meter and tx histogram.
    #[test]
    fn test_record_ledger_close() {
        let compat = MedidaCompat::new();
        compat.record_ledger_close(120.0, 50);
        compat.record_ledger_close(80.0, 30);
        let timer = compat.close_timer.snapshot();
        assert_eq!(timer.count, 2);
        assert_eq!(timer.min, 80.0);
        assert_eq!(timer.max, 120.0);
        assert_eq!(compat.close_meter.snapshot().count, 2);
        let hist = compat.tx_count_histogram.snapshot();
        assert_eq!(hist.count, 2);
        assert_eq!(hist.min, 30.0);
        assert_eq!(hist.max, 50.0);
    }
}
