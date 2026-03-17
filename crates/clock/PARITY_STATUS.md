# stellar-core Parity Status

**Crate**: `henyey-clock`
**Upstream**: `stellar-core/src/util/Timer.h`, `stellar-core/src/util/Timer.cpp`
**Overall Parity**: 100%
**Last Updated**: 2026-03-17

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Steady-time queries (`now()`) | Full | `Clock::now()` returns `Instant` |
| System-time queries (`system_now()`) | Full | `Clock::system_now()` returns `SystemTime` |
| Real vs virtual mode separation | Full | `RealClock` / `VirtualClock` split |
| Async sleep / delay | Full | `Clock::sleep()` wraps `tokio::time::sleep` |
| Periodic intervals | Full | `Clock::interval()` wraps `tokio::time::interval` |
| Virtual time advancement | Full | `VirtualClock::set_base_instant()` |
| Event loop / IO context | N/A | Intentional omission — tokio runtime |
| Timer management (VirtualTimer) | N/A | Intentional omission — tokio timers |
| Time conversion utilities | N/A | Intentional omission — Rust stdlib |
| Scheduler integration | N/A | Intentional omission — tokio scheduler |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Timer.h` / `Timer.cpp` | `lib.rs` | Core timing trait + two implementations |

## Component Mapping

### lib.rs (`Clock` trait, `RealClock`, `VirtualClock`)

Corresponds to: `Timer.h` — `VirtualClock` class (public API)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VirtualClock::now()` | `Clock::now()` | Full |
| `VirtualClock::system_now()` | `Clock::system_now()` | Full |
| `VirtualClock::sleep_for()` | `Clock::sleep()` | Full |
| `VirtualClock::setCurrentVirtualTime()` | `VirtualClock::set_base_instant()` | Full |
| `VirtualClock(REAL_TIME)` | `RealClock` | Full |
| `VirtualClock(VIRTUAL_TIME)` | `VirtualClock` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `VirtualClock::crank()` | Event loop driven by tokio runtime, not manual cranking |
| `VirtualClock::getIOContext()` | Tokio provides the async executor |
| `VirtualClock::postAction()` | Replaced by `tokio::spawn` / channel-based message passing |
| `VirtualClock::enqueue()` | Timer event queue managed by tokio |
| `VirtualClock::flushCancelledEvents()` | Tokio drop semantics handle cleanup |
| `VirtualClock::cancelAllEvents()` | Tokio task cancellation via `abort()` / drop |
| `VirtualClock::advanceToNext()` | Virtual time auto-advances in tokio; no manual cranking |
| `VirtualClock::advanceToNow()` | Virtual time auto-advances in tokio; no manual cranking |
| `VirtualClock::maybeSetRealtimer()` | Internal to event loop; tokio handles real-time dispatch |
| `VirtualClock::shutdown()` | Tokio runtime shutdown / graceful cancellation |
| `VirtualClock::isStopped()` | Tokio runtime state management |
| `VirtualClock::shouldYield()` | Cooperative scheduling handled by tokio's task system |
| `VirtualClock::getMode()` | Mode encoded in type system (`RealClock` vs `VirtualClock`) |
| `VirtualClock::newBackgroundWork()` / `finishedBackgroundWork()` | Tokio task model tracks outstanding work |
| `VirtualClock::next()` | Internal to event loop dispatch |
| `VirtualClock::getActionQueueSize()` | Scheduler monitoring handled by tokio metrics |
| `VirtualClock::actionQueueIsOverloaded()` | Scheduler monitoring handled by tokio metrics |
| `VirtualClock::currentSchedulerActionType()` | Scheduler type tracking not needed with tokio |
| `Scheduler` class | Replaced by tokio's built-in fair scheduling |
| `VirtualTimer` class (all methods) | Replaced by `tokio::time::sleep` / `tokio::time::interval` |
| `VirtualClockEvent` class (all methods) | Internal event representation; tokio manages timer events |
| `VirtualClock::to_time_t()` | Rust `SystemTime` / `chrono` handles time conversions |
| `VirtualClock::from_time_t()` | Rust `SystemTime` / `chrono` handles time conversions |
| `VirtualClock::systemPointToTm()` | Rust `chrono` / standard library |
| `VirtualClock::tmToSystemPoint()` | Rust `chrono` / standard library |
| `VirtualClock::isoStringToTm()` | Rust `chrono` / standard library |
| `VirtualClock::tmToISOString()` | Rust `chrono` / standard library |
| `VirtualClock::systemPointToISOString()` | Rust `chrono` / standard library |

## Gaps

No known gaps.

## Architectural Differences

1. **Event loop model**
   - **stellar-core**: `VirtualClock` owns an `asio::io_context` and a `Scheduler`, manually cranking through events, timers, and IO in a single-threaded loop.
   - **Rust**: The tokio runtime provides the async executor. `Clock` is a pure timing trait with no event loop responsibility.
   - **Rationale**: Tokio is a mature, production-grade async runtime. Reimplementing event dispatch would add complexity with no behavioral benefit.

2. **Timer management**
   - **stellar-core**: `VirtualTimer` and `VirtualClockEvent` form a priority-queue-based timer system integrated with `VirtualClock`.
   - **Rust**: `tokio::time::sleep` and `tokio::time::interval` provide equivalent timer functionality.
   - **Rationale**: Tokio timers are well-tested and integrate naturally with `async`/`await`.

3. **Mode encoding**
   - **stellar-core**: A single `VirtualClock` class with a runtime `Mode` enum (`REAL_TIME` / `VIRTUAL_TIME`).
   - **Rust**: Separate types `RealClock` and `VirtualClock` both implementing the `Clock` trait. Mode is encoded at the type level.
   - **Rationale**: Type-level separation enables compile-time guarantees and cleaner dependency injection.

4. **Time conversion utilities**
   - **stellar-core**: `VirtualClock` bundles static time conversion methods (`to_time_t`, `from_time_t`, ISO string conversions).
   - **Rust**: Standard library `SystemTime` and the `chrono` crate handle these conversions.
   - **Rationale**: Rust's standard library and ecosystem provide these utilities without custom wrappers.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Time conversions | 4 TEST_CASE | 0 #[test] | Handled by Rust stdlib; no custom wrappers to test |
| Virtual event dispatch | 1 TEST_CASE | 0 #[test] | Tokio timer dispatch; no custom event loop |
| Shared clock / multi-app | 1 TEST_CASE | 0 #[test] | Not applicable to trait-based design |
| Timer cancellation | 1 TEST_CASE | 0 #[test] | Tokio cancellation via drop |
| Background work tracking | 1 TEST_CASE (Postgres-only) | 0 #[test] | Not applicable |
| Monotonic time | 0 | 1 #[test] | `real_clock_returns_monotonic_now` |
| Virtual clock base instant | 0 | 1 #[test] | `virtual_clock_base_instant_controls_now` |
| Async sleep | 0 | 1 #[test] | `sleep_completes` |
| Interval ticking | 0 | 1 #[test] | `interval_yields_ticks` |

### Test Gaps

The upstream tests primarily exercise the event loop, timer management, and time conversion subsystems — all of which are intentionally omitted from this crate (handled by tokio and Rust stdlib). The Rust tests cover the trait-specific functionality that has no upstream equivalent (trait implementations, async sleep, intervals). No meaningful test gap exists given the architectural differences.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 6 |
| Gaps (None + Partial) | 0 |
| Intentional Omissions | 28 |
| **Parity** | **6 / (6 + 0) = 100%** |
