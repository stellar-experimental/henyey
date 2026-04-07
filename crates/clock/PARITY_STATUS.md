# stellar-core Parity Status

**Crate**: `henyey-clock`
**Upstream**: `stellar-core/src/util/Timer.h`, `stellar-core/src/util/Timer.cpp`
**Overall Parity**: 100%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Monotonic time reads | Full | `Clock::now()` covers the steady-clock subset |
| Wall-clock reads | Full | `Clock::system_now()` exposes wall-clock access |
| Real vs virtual mode selection | Full | Split into `RealClock` and `VirtualClock` types |
| Delay primitive | Full | `Clock::sleep()` covers the in-scope wait API |
| Periodic intervals | Full | Rust-only convenience on top of async timers |
| Manual virtual-time stepping | None | Intentionally omitted; no crankable simulator clock |
| Event loop and scheduler control | None | Intentionally delegated to tokio |
| Explicit timer objects and cancellation | None | Intentionally replaced by futures and streams |
| Time conversion helpers | None | Intentionally delegated to stdlib and `chrono` |
| Background-work accounting | None | Intentionally not modeled in the trait |

This crate intentionally mirrors only the timing facade from stellar-core's
`VirtualClock`. The upstream event loop, scheduler, timer queue, and calendar
time conversion helpers are excluded by design and therefore do not count
against the parity percentage.

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Timer.h` / `Timer.cpp` | `src/lib.rs` | Trait-only timing subset of `VirtualClock` |

## Component Mapping

### clock_surface (`src/lib.rs`)

Corresponds to: `Timer.h` (`VirtualClock` public API)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VirtualClock::to_time_t()` | stdlib / `chrono` | None |
| `VirtualClock::from_time_t()` | stdlib / `chrono` | None |
| `VirtualClock::systemPointToTm()` | stdlib / `chrono` | None |
| `VirtualClock::tmToSystemPoint()` | stdlib / `chrono` | None |
| `VirtualClock::isoStringToTm()` | stdlib / `chrono` | None |
| `VirtualClock::tmToISOString()` | stdlib / `chrono` | None |
| `VirtualClock::systemPointToISOString()` | stdlib / `chrono` | None |
| `VirtualClock::shouldYield()` | tokio scheduler | None |
| `VirtualClock::shutdown()` | tokio runtime shutdown | None |
| `VirtualClock::isStopped()` | tokio runtime state | None |
| `VirtualClock::getMode()` | type-level split (`RealClock` / `VirtualClock`) | None |
| `VirtualClock::newBackgroundWork()` | task bookkeeping outside this crate | None |
| `VirtualClock::finishedBackgroundWork()` | task bookkeeping outside this crate | None |
| `VirtualClock::VirtualClock(Mode)` | `RealClock`, `VirtualClock::new()` | Full |
| `VirtualClock::crank()` | tokio executor | None |
| `VirtualClock::getIOContext()` | tokio runtime | None |
| `VirtualClock::now()` | `Clock::now()` | Full |
| `VirtualClock::system_now()` | `Clock::system_now()` | Full |
| `VirtualClock::enqueue()` | tokio task scheduling | None |
| `VirtualClock::flushCancelledEvents()` | tokio drop semantics | None |
| `VirtualClock::cancelAllEvents()` | task / future cancellation outside this crate | None |
| `VirtualClock::setCurrentVirtualTime(time_point)` | no equivalent | None |
| `VirtualClock::setCurrentVirtualTime(system_time_point)` | no equivalent | None |
| `VirtualClock::sleep_for()` | `Clock::sleep()` | Full |
| `VirtualClock::next()` | no equivalent | None |
| `VirtualClock::postAction()` | tokio task spawning / channels | None |
| `VirtualClock::getActionQueueSize()` | no equivalent | None |
| `VirtualClock::actionQueueIsOverloaded()` | no equivalent | None |
| `VirtualClock::currentSchedulerActionType()` | no equivalent | None |

### timer_event_surface (`src/lib.rs`)

Corresponds to: `Timer.h` (`VirtualClockEvent` public API)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VirtualClockEvent::VirtualClockEvent()` | no standalone event type | None |
| `VirtualClockEvent::getTriggered()` | no standalone event type | None |
| `VirtualClockEvent::trigger()` | no standalone event type | None |
| `VirtualClockEvent::cancel()` | no standalone event type | None |
| `VirtualClockEvent::operator<()` | no standalone event type | None |

### timer_handle_surface (`src/lib.rs`)

Corresponds to: `Timer.h` (`VirtualTimer` public API)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VirtualTimer::VirtualTimer(Application&)` | `Clock::sleep()` / `Clock::interval()` | None |
| `VirtualTimer::VirtualTimer(VirtualClock&)` | `Clock::sleep()` / `Clock::interval()` | None |
| `VirtualTimer::expiry_time()` | no equivalent | None |
| `VirtualTimer::seq()` | no equivalent | None |
| `VirtualTimer::expires_at()` | no equivalent | None |
| `VirtualTimer::expires_from_now()` | `Clock::sleep()` / `Clock::interval()` | None |
| `VirtualTimer::async_wait(error_code)` | `Clock::sleep()` / `Clock::interval()` | None |
| `VirtualTimer::async_wait(success, failure)` | no equivalent | None |
| `VirtualTimer::cancel()` | no equivalent | None |
| `VirtualTimer::onFailureNoop()` | no equivalent | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| Time conversion helpers (7 methods) | Rust standard library and `chrono` already provide these conversions |
| Scheduler and event-loop controls (12 methods) | Tokio owns execution, wakeups, and queue management |
| Mode and background-work bookkeeping (3 methods) | Runtime mode becomes type selection; task accounting lives elsewhere |
| Manual virtual-time stepping (2 methods) | This crate intentionally does not expose a crankable simulation clock |
| `VirtualClockEvent` API (5 methods) | Tokio manages timer events internally rather than exposing event nodes |
| `VirtualTimer` API (10 methods) | Delays are modeled as futures and streams instead of timer objects |
| `VirtualClock::next()` | No public timer-queue inspection surface in the Rust crate |

## Gaps

No known gaps.

## Architectural Differences

1. **Clock scope**
   - **stellar-core**: `VirtualClock` is both a clock and the owner of the process-local async event loop.
   - **Rust**: `Clock` is only a timing trait; async execution comes from the surrounding tokio runtime.
   - **Rationale**: Henyey centralizes executor concerns in tokio instead of rebuilding them inside a utility crate.

2. **Mode representation**
   - **stellar-core**: One class switches behavior with a runtime `Mode` enum.
   - **Rust**: `RealClock` and `VirtualClock` are separate types behind the same trait.
   - **Rationale**: Type-level mode selection keeps dependency injection simple and avoids runtime branching for callers.

3. **Timer API shape**
   - **stellar-core**: Timers are explicit `VirtualTimer` objects backed by queued `VirtualClockEvent`s.
   - **Rust**: Delays are exposed as futures (`sleep`) and streams (`interval`) using tokio primitives.
   - **Rationale**: This matches idiomatic async Rust and removes the need for a bespoke timer object layer.

4. **Simulation model**
   - **stellar-core**: Virtual time advances through `crank()` and explicit `setCurrentVirtualTime()` calls.
   - **Rust**: `VirtualClock` is an anchored monotonic clock, not a manually stepped simulator clock.
   - **Rationale**: The crate is scoped to injectable timing reads and waits, not full simulation orchestration.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Time conversion helpers | 5 TEST_CASE / 0 SECTION | 0 `#[test]` | Omitted; conversions rely on stdlib / `chrono` |
| Background-work and virtual-time coordination | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | Omitted with the event-loop surface |
| Virtual event dispatch and shared clock behavior | 2 TEST_CASE / 0 SECTION | 0 `#[test]` | Omitted; no crankable shared scheduler |
| Timer cancellation | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | Omitted; no explicit timer handle API |
| Core clock trait behavior | 0 TEST_CASE / 0 SECTION | 4 `#[test]` | Covers `now`, `sleep`, and `interval` primitives |

### Test Gaps

The upstream test suite is dominated by event-loop, shared-clock, and timer
queue behavior that this crate intentionally leaves to tokio. The Rust tests
cover the in-scope API surface, but there is no crate-local coverage for the
omitted simulator features because they are not implemented here.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 4 |
| Gaps (None + Partial) | 0 |
| Intentional Omissions | 40 |
| **Parity** | **4 / (4 + 0) = 100%** |
