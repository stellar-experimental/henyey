# stellar-core Parity Status

**Crate**: `henyey-work`
**Upstream**: `stellar-core/src/work/`
**Overall Parity**: 25%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Basic work lifecycle | Partial | Core run/retry/cancel flow exists; lifecycle hooks missing |
| Scheduler execution loop | Full | Async ready-queue scheduler implemented |
| Dependency ordering | Full | Explicit DAG edges enforce ordering |
| Retry management | Partial | Retry delays exist; named constants and hooks differ |
| Cancellation handling | Partial | Cooperative cancel, no `ABORTING` phase |
| Hierarchical work tree | None | `Work` parent-child supervision is absent |
| WorkSequence helper | None | Not implemented (no `sequence.rs`) |
| BatchWork coordinator | None | `BatchWork` equivalent missing |
| ConditionalWork gating | None | `ConditionalWork` equivalent missing |
| WorkWithCallback wrapper | None | Not implemented (no `callback.rs`) |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `BasicWork.h` / `BasicWork.cpp` | `types.rs`, `scheduler.rs` | States/outcomes in `types.rs`; execution, retries, and cancellation in `scheduler.rs` |
| `Work.h` / `Work.cpp` | (not mapped) | Flat DAG scheduler replaces hierarchical parent-child work |
| `WorkScheduler.h` / `WorkScheduler.cpp` | `scheduler.rs` | Scheduler loop, concurrency control, retries, metrics, and events |
| `WorkSequence.h` / `WorkSequence.cpp` | (not mapped) | Sequential execution helper not implemented |
| `WorkWithCallback.h` / `WorkWithCallback.cpp` | (not mapped) | Callback wrapper not implemented |
| `BatchWork.h` / `BatchWork.cpp` | (not mapped) | Parallel batch coordinator not implemented |
| `ConditionalWork.h` / `ConditionalWork.cpp` | (not mapped) | Condition-gated work wrapper not implemented |

## Component Mapping

### Basic work model (`types.rs`, `scheduler.rs`)

Corresponds to: `BasicWork.h` / `BasicWork.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BasicWork::State` enum (5 values) | `WorkOutcome` enum (4 variants) | Full |
| `BasicWork::InternalState` enum (8 values) | `WorkState` enum (6 variants) | Full |
| `BasicWork()` constructor | `Work` trait + scheduler entry construction | Full |
| `getName()` | `Work::name()` | Full |
| `getState()` | `WorkScheduler::state()` | Full |
| `getStatus()` formatted string | Not implemented | None |
| `isDone()` | `WorkState::is_terminal()` | Full |
| `crankWork()` | Tokio task execution inside scheduler loop | Full |
| `startWork()` | Scheduler starts work when runnable | Full |
| `onRun()` pure virtual | `Work::run()` async method | Full |
| `onAbort()` pure virtual | Cooperative cancellation via `WorkContext` | Full |
| `shutdown()` | `WorkScheduler::cancel()` / `cancel_all()` | Full |
| `isAborting()` | `CancellationToken::is_cancelled()` | Full |
| `RETRY_NEVER` / `RETRY_ONCE` / `RETRY_A_FEW` / `RETRY_A_LOT` | Caller passes `retries: u32` | Partial |
| `onSuccess()` callback | Not implemented | None |
| `onFailureRetry()` callback | Not implemented | None |
| `onFailureRaise()` callback | Not implemented | None |
| `getRetryDelay()` exponential backoff | Work decides delay via `WorkOutcome::Retry { delay }` | Partial |
| `getRetryETA()` | Not implemented | None |

### Hierarchical work tree (not implemented)

Corresponds to: `Work.h` / `Work.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Work()` constructor | (not implemented) | None |
| `getStatus()` with child counts | (not implemented) | None |
| `allChildrenSuccessful()` | (not implemented) | None |
| `allChildrenDone()` | (not implemented) | None |
| `anyChildRaiseFailure()` | (not implemented) | None |
| `anyChildRunning()` | (not implemented) | None |
| `hasChildren()` | (not implemented) | None |
| `shutdown()` with child shutdown | (not implemented) | None |
| `addWork<T>()` template | (not implemented) | None |
| `addWorkWithCallback<T>()` template | (not implemented) | None |
| `addWork(cb, child)` | (not implemented) | None |
| `onRun()` with round-robin child dispatch | (not implemented) | None |
| `onAbort()` with child abort propagation | (not implemented) | None |
| `onReset()` with child cleanup | (not implemented) | None |
| `doWork()` pure virtual | (not implemented) | None |
| `doReset()` virtual | (not implemented) | None |
| `checkChildrenStatus()` | (not implemented) | None |
| `yieldNextRunningChild()` | (not implemented) | None |
| `WorkUtils::getWorkStatus()` | (not implemented) | None |
| `WorkUtils::allSuccessful()` | (not implemented) | None |
| `WorkUtils::anyFailed()` | (not implemented) | None |
| `WorkUtils::anyRunning()` | (not implemented) | None |

### Scheduler (`scheduler.rs`)

Corresponds to: `WorkScheduler.h` / `WorkScheduler.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkScheduler()` constructor | `WorkScheduler::new()` | Full |
| `create()` factory | `WorkScheduler::new()` | Full |
| `executeWork<T>()` blocking run | `run_until_done()` | Full |
| `scheduleWork<T>()` non-blocking scheduling | `add_work()` + `run_until_done()` | Full |
| `shutdown()` | `cancel_all()` | Full |
| `scheduleOne()` IO-posted crank | Tokio task spawning / ready queue | Full |
| `doWork()` scheduler loop | `run_until_done_with_cancel()` | Full |

### Sequence helper (not implemented)

Corresponds to: `WorkSequence.h` / `WorkSequence.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkSequence()` constructor | (not implemented) | None |
| `getStatus()` | (not implemented) | None |
| `shutdown()` | (not implemented) | None |
| `onRun()` sequential dispatch | (not implemented) | None |
| `onAbort()` | (not implemented) | None |
| `onReset()` | (not implemented) | None |
| `stopAtFirstFailure` flag | (not implemented) | None |

### Batch coordinator (not implemented)

Corresponds to: `BatchWork.h` / `BatchWork.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BatchWork()` constructor | (not implemented) | None |
| `getNumWorksInBatch()` | (not implemented) | None |
| `doReset()` | (not implemented) | None |
| `doWork()` batch coordination | (not implemented) | None |
| `hasNext()` pure virtual | (not implemented) | None |
| `yieldMoreWork()` pure virtual | (not implemented) | None |
| `resetIter()` pure virtual | (not implemented) | None |
| `addMoreWorkIfNeeded()` | (not implemented) | None |

### Conditional wrapper (not implemented)

Corresponds to: `ConditionalWork.h` / `ConditionalWork.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ConditionalWork()` constructor | (not implemented) | None |
| `shutdown()` | (not implemented) | None |
| `getStatus()` | (not implemented) | None |
| `onRun()` condition polling | (not implemented) | None |
| `onAbort()` | (not implemented) | None |
| `onReset()` | (not implemented) | None |

### Callback wrapper (not implemented)

Corresponds to: `WorkWithCallback.h` / `WorkWithCallback.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkWithCallback()` constructor | (not implemented) | None |
| `onRun()` callback execution | (not implemented) | None |
| `onAbort()` | (not implemented) | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `VirtualClock` / `VirtualTimer` integration | Tokio async timers replace virtual-clock callbacks |
| `postOnMainThread` scheduling | Tokio task spawning replaces main-thread posting |
| `shouldYield()` cooperative yielding | Async tasks yield naturally at `.await` points |
| `wakeUp()` / `wakeSelfUpCallback()` / `setupWaitingCallback()` | Async/await eliminates explicit wake-up plumbing |
| `onReset()` lifecycle hook in `BasicWork` | Work retains state directly across retries; no central reset hook |
| `TRIGGER_PERIOD` constant | Event-driven async scheduler does not poll on a fixed period |
| `mApp` application context on work items | Rust passes only `WorkContext`; broader state stays outside the scheduler |
| Tracy `ZoneScoped` profiling hooks | Not required for correctness; `tracing` can cover profiling separately |
| `CLOG_*` logging macros | Rust uses `tracing` instead |
| `NonMovableOrCopyable` base | Rust ownership already enforces non-copyable semantics |
| `enable_shared_from_this` patterns | Ownership model avoids `shared_ptr` self-references |
| `ALLOWED_TRANSITIONS` / `assertValidTransition()` | Simpler state model avoids explicit transition tables |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `Work` class hierarchy | High | Missing parent-child supervision blocks parity for composite work trees |
| `WorkSequence` | High | Sequential execution helper is absent |
| `BatchWork` | Medium | Parallel batch throttling helper is absent |
| `ConditionalWork` | Medium | Condition-gated sequential dependencies are unsupported |
| `WorkWithCallback` | Medium | Simple callback wrapper is absent |
| `onSuccess()` lifecycle hook | Low | No post-success hook |
| `onFailureRetry()` lifecycle hook | Low | Retry-side callbacks do not exist |
| `onFailureRaise()` lifecycle hook | Low | Permanent-failure callback hook does not exist |
| `getRetryETA()` | Low | No ETA reporting for delayed retries |
| `getStatus()` formatted strings | Low | No formatted status API |
| `RETRY_*` named constants | Low | Callers use numeric retry budgets directly |

## Architectural Differences

1. **Execution model**
   - **stellar-core**: Cooperative single-threaded cranking through the main IO loop with explicit waiting and wake-up callbacks.
   - **Rust**: Tokio runs work as async tasks, and waiting is expressed with `.await` rather than explicit `WORK_WAITING` state transitions.
   - **Rationale**: Async tasks simplify scheduler control flow and remove most timer-plumbing code.

2. **Hierarchy vs. flat DAG**
   - **stellar-core**: `Work` builds trees of parent and child work items, and the scheduler walks those trees in round-robin order.
   - **Rust**: `WorkScheduler` owns a flat graph of work items with explicit dependency edges.
   - **Rationale**: Current henyey workflows are easier to express as dependency graphs than nested supervision trees.

3. **Cancellation semantics**
   - **stellar-core**: `shutdown()` moves work through `ABORTING` and `ABORTED` states while `onAbort()` actively stops in-flight work.
   - **Rust**: Cancellation is cooperative through `CancellationToken`, and the scheduler marks work cancelled immediately.
   - **Rationale**: Cooperative cancellation matches Tokio tasks well, but it omits the richer intermediate abort lifecycle.

4. **Retry strategy**
   - **stellar-core**: `BasicWork` owns retry counters, exponential backoff, and retry/failure callbacks.
   - **Rust**: Scheduler tracks retry budgets, while each work item chooses its own retry delay by returning `WorkOutcome::Retry { delay }`.
   - **Rationale**: This keeps retry policy local to each task, but it drops several built-in lifecycle hooks.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| `BasicWork` state machine | 1 TEST_CASE / 8 SECTION | 0 `#[test]` | No direct coverage for waiting, retry hooks, or shutdown transitions |
| Hierarchical `Work` trees | 1 TEST_CASE / 4 SECTION | 0 `#[test]` | Not implemented in Rust |
| Scheduler ordering and retries | 2 TEST_CASE / 4 SECTION | 2 `#[tokio::test]` | Covers dependency order and retry-then-success |
| `WorkSequence` | 1 TEST_CASE / 5 SECTION | 0 `#[test]` | Not implemented in Rust |
| `BatchWork` | 1 TEST_CASE / 2 SECTION | 0 `#[test]` | Not implemented in Rust |
| `ConditionalWork` | 1 TEST_CASE / 5 SECTION | 0 `#[test]` | Not implemented in Rust |
| `RunCommandWork` | 1 TEST_CASE / 4 SECTION | 0 `#[test]` | Not part of work crate |
| Cancellation and metrics | Shutdown scenarios across multiple sections | 2 `#[tokio::test]` | Covers cancel + metrics/snapshot |

### Test Gaps

- No Rust tests exercise the `BasicWork` state machine behaviors (waiting, abort transitions, retry hooks).
- Rust tests do not cover fairness or multi-level scheduling comparable to stellar-core's hierarchical tests.
- No Rust equivalents exist for upstream `WorkSequence`, `BatchWork`, `ConditionalWork`, or `WorkWithCallback` tests because those abstractions are missing.
- Only 4 `#[tokio::test]` tests exist vs. 8 TEST_CASE / 32 SECTION upstream.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 18 |
| Gaps (None + Partial) | 53 |
| Intentional Omissions | 12 |
| **Parity** | **18 / (18 + 53) = 25%** |
