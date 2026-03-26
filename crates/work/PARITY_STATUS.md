# stellar-core Parity Status

**Crate**: `henyey-work`
**Upstream**: `stellar-core/src/work/`
**Overall Parity**: 39%
**Last Updated**: 2026-03-25

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Basic work lifecycle | Partial | Core run/retry/cancel flow exists |
| Scheduler execution loop | Full | Async ready-queue scheduler implemented |
| Dependency ordering | Full | Explicit DAG edges enforce ordering |
| Retry management | Partial | Retry delays exist; hooks/constants differ |
| Cancellation handling | Partial | Cooperative cancel, no `ABORTING` phase |
| WorkSequence helper | Full | Sequential chains via dependency edges |
| WorkWithCallback wrapper | Full | Post-run callback wrapper exists |
| Hierarchical work tree | None | Parent-child supervision is absent |
| Batch coordination | None | `BatchWork` equivalent missing |
| Conditional gating | None | `ConditionalWork` equivalent missing |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `BasicWork.h` / `BasicWork.cpp` | `types.rs`, `scheduler.rs` | States/outcomes live in `types.rs`; execution, retries, and cancellation live in `scheduler.rs` |
| `Work.h` / `Work.cpp` | (not mapped) | Flat DAG scheduler replaces hierarchical parent-child work |
| `WorkScheduler.h` / `WorkScheduler.cpp` | `scheduler.rs` | Scheduler loop, concurrency control, retries, metrics, and events |
| `WorkSequence.h` / `WorkSequence.cpp` | `sequence.rs` | Helper builds a linear dependency chain |
| `WorkWithCallback.h` / `WorkWithCallback.cpp` | `callback.rs` | Wrapper runs callback after each attempt |
| `BatchWork.h` / `BatchWork.cpp` | (not mapped) | Parallel batch coordinator not implemented |
| `ConditionalWork.h` / `ConditionalWork.cpp` | (not mapped) | Condition-gated work wrapper not implemented |

## Component Mapping

### Basic work model (`types.rs`, `scheduler.rs`)

Corresponds to: `BasicWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BasicWork::State` enum (5 values) | `WorkOutcome` enum (4 variants) | Full |
| `BasicWork::InternalState` enum (8 values) | `WorkState` enum (6 variants) | Full |
| `BasicWork()` constructor | `Work` trait + scheduler entry construction | Full |
| `getName()` | `Work::name()` | Full |
| `getState()` | `WorkScheduler::state()` | Full |
| `isDone()` | `WorkState::is_terminal()` | Full |
| `onRun()` pure virtual | `Work::run()` async method | Full |
| `startWork()` | Scheduler starts work when runnable | Full |
| `crankWork()` | Tokio task execution inside scheduler loop | Full |
| `RETRY_NEVER` / `RETRY_ONCE` / `RETRY_A_FEW` / `RETRY_A_LOT` | Caller passes `retries: u32` | Partial |
| `getStatus()` | `WorkSnapshot` / `WorkEvent` introspection | Partial |
| `onAbort()` pure virtual | Cooperative cancellation via `WorkContext` | Full |
| `shutdown()` | `WorkScheduler::cancel()` / `cancel_all()` | Full |
| `isAborting()` | `CancellationToken::is_cancelled()` | Full |
| `onSuccess()` callback | Not implemented; use `WorkWithCallback` | None |
| `onFailureRetry()` callback | Not implemented | None |
| `onFailureRaise()` callback | Not implemented | None |
| `getRetryDelay()` exponential backoff | Work decides delay via `WorkOutcome::Retry { delay }` | Partial |
| `getRetryETA()` | Not implemented | None |

### Hierarchical work tree (not implemented)

Corresponds to: `Work.h`

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

Corresponds to: `WorkScheduler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkScheduler()` constructor | `WorkScheduler::new()` | Full |
| `create()` factory | `WorkScheduler::new()` | Full |
| `executeWork<T>()` blocking run | `run_until_done()` | Full |
| `scheduleWork<T>()` non-blocking scheduling | `add_work()` + `run_until_done()` | Full |
| `shutdown()` | `cancel_all()` | Full |
| `scheduleOne()` IO-posted crank | Tokio task spawning / ready queue | Full |
| `doWork()` scheduler loop | `run_until_done_with_cancel()` | Full |

### Sequence helper (`sequence.rs`)

Corresponds to: `WorkSequence.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkSequence()` constructor | `WorkSequence::new()` | Full |
| `onRun()` sequential dispatch | `push()` creates a dependency chain | Full |
| `onAbort()` abort current work | Scheduler cancellation blocks downstream work | Full |
| `onReset()` | Not needed; scheduler retries original work items | Full |
| `getStatus()` | Not implemented; inspect scheduler snapshots instead | Partial |
| `shutdown()` | Scheduler cancellation APIs | Full |
| `stopAtFirstFailure` flag | Failed dependency blocks later sequence entries | Full |

### Batch coordinator (not implemented)

Corresponds to: `BatchWork.h`

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

Corresponds to: `ConditionalWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ConditionalWork()` constructor | (not implemented) | None |
| `shutdown()` | (not implemented) | None |
| `getStatus()` | (not implemented) | None |
| `onRun()` condition polling | (not implemented) | None |
| `onAbort()` | (not implemented) | None |
| `onReset()` | (not implemented) | None |

### Callback wrapper (`callback.rs`)

Corresponds to: `WorkWithCallback.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkWithCallback()` constructor | `WorkWithCallback::new()` | Full |
| `onRun()` callback execution | Wrapped `Work::run()` plus callback invocation | Full |
| `onAbort()` | Cancellation token propagation | Full |

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
| `BatchWork` | Medium | Parallel batch throttling helper is absent |
| `ConditionalWork` | Medium | Condition-gated sequential dependencies are unsupported |
| `onSuccess()` lifecycle hook | Low | Callers must wrap work explicitly with `WorkWithCallback` |
| `onFailureRetry()` lifecycle hook | Low | Retry-side callbacks do not exist |
| `onFailureRaise()` lifecycle hook | Low | Permanent-failure callback hook does not exist |
| `getRetryETA()` | Low | No ETA reporting for delayed retries |
| `getStatus()` formatted strings | Low | Introspection exists, but no formatted status API |
| `RETRY_*` retry constants | Low | Callers use numeric retry budgets directly |

## Architectural Differences

1. **Execution model**
   - **stellar-core**: Cooperative single-threaded cranking through the main IO loop with explicit waiting and wake-up callbacks.
   - **Rust**: Tokio runs work as async tasks, and waiting is expressed with `.await` rather than explicit `WORK_WAITING` state transitions.
   - **Rationale**: Async tasks simplify scheduler control flow and remove most timer-plumbing code.

2. **Hierarchy vs. flat DAG**
   - **stellar-core**: `Work` builds trees of parent and child work items, and the scheduler walks those trees in round-robin order.
   - **Rust**: `WorkScheduler` owns a flat graph of work items with explicit dependency edges; `WorkSequence` only builds those edges.
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
| `WorkSequence` | 1 TEST_CASE / 5 SECTION | 1 `#[tokio::test]` | Only basic sequential ordering is covered |
| `WorkWithCallback` | Tested inline in `WorkTests.cpp` | 1 `#[tokio::test]` | Basic callback invocation covered |
| `BatchWork` | 1 TEST_CASE / 2 SECTION | 0 `#[test]` | Feature absent |
| `ConditionalWork` | 1 TEST_CASE / 5 SECTION | 0 `#[test]` | Feature absent |
| Cancellation and introspection | Shutdown scenarios spread across multiple sections | 2 `#[tokio::test]` | Covers external cancellation plus metrics/snapshot reporting |

### Test Gaps

- No Rust test exercises the richer `BasicWork` state machine behaviors that stellar-core covers, especially waiting callbacks, abort transitions, and retry callback hooks.
- Rust tests do not cover fairness or multi-level scheduling behavior comparable to stellar-core's hierarchical scheduling tests.
- `WorkSequence` has only a happy-path ordering test; upstream also checks empty sequences, mid-sequence failure, and shutdown behavior.
- No Rust equivalents exist for upstream `BatchWork` and `ConditionalWork` tests because those abstractions are missing.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 28 |
| Gaps (None + Partial) | 44 |
| Intentional Omissions | 12 |
| **Parity** | **28 / (28 + 44) = 39%** |
