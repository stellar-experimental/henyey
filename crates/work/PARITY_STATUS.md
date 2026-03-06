# stellar-core Parity Status

**Crate**: `henyey-work`
**Upstream**: `stellar-core/src/work/`
**Overall Parity**: 39%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Work trait / BasicWork abstraction | Full | Async trait replaces class hierarchy |
| State machine (WorkState / State enum) | Full | Simplified from 8 to 6 states |
| Retry mechanism | Full | Outcome-based rather than internal counter |
| Cancellation / abort | Full | CancellationToken replaces shutdown/abort |
| Work scheduler core loop | Full | DAG-based async execution |
| Concurrency control | Full | max_concurrency config parameter |
| Dependency ordering | Full | Explicit DAG deps replace hierarchical model |
| Work sequence helper | Full | Different design, same sequential semantics |
| WorkWithCallback wrapper | Full | Wraps work + callback vs callback-as-work |
| Event monitoring | Full | Channel-based WorkEvent system |
| Metrics / introspection | Full | metrics() and snapshot() APIs |
| Hierarchical work (Work class) | None | Flat DAG replaces parent-child tree |
| BatchWork parallel batching | None | Not implemented |
| ConditionalWork gated execution | None | Not implemented |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `BasicWork.h` / `BasicWork.cpp` | `lib.rs` | `Work` trait, `WorkOutcome`, `WorkState`, `WorkContext` |
| `Work.h` / `Work.cpp` | (not mapped) | Hierarchical work model not implemented |
| `WorkScheduler.h` / `WorkScheduler.cpp` | `lib.rs` | `WorkScheduler`, `WorkSchedulerConfig` |
| `WorkSequence.h` / `WorkSequence.cpp` | `lib.rs` | `WorkSequence` helper struct |
| `WorkWithCallback.h` / `WorkWithCallback.cpp` | `lib.rs` | `WorkWithCallback` wrapper struct |
| `BatchWork.h` / `BatchWork.cpp` | (not mapped) | Not implemented |
| `ConditionalWork.h` / `ConditionalWork.cpp` | (not mapped) | Not implemented |

## Component Mapping

### Work trait and state types (`lib.rs`)

Corresponds to: `BasicWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BasicWork::State` enum (5 values) | `WorkOutcome` enum (4 variants) | Full |
| `BasicWork::InternalState` enum (8 values) | `WorkState` enum (6 variants) | Full |
| `BasicWork()` constructor | `Work` trait + `WorkEntry` construction | Full |
| `getName()` | `Work::name()` | Full |
| `getState()` | `WorkScheduler::state()` | Full |
| `isDone()` | `WorkState::is_terminal()` | Full |
| `onRun()` pure virtual | `Work::run()` async method | Full |
| `startWork()` | Handled by scheduler when spawning | Full |
| `crankWork()` | Handled by Tokio async execution | Full |
| `RETRY_NEVER` / `RETRY_ONCE` / `RETRY_A_FEW` / `RETRY_A_LOT` | Caller passes `retries: u32` | Partial |
| `getStatus()` formatted string | `WorkSnapshot` / `WorkEvent` | Partial |
| `onAbort()` pure virtual | `WorkContext::is_cancelled()` cooperative | Full |
| `shutdown()` | `WorkScheduler::cancel()` / `cancel_all()` | Full |
| `isAborting()` | `CancellationToken::is_cancelled()` | Full |
| `onSuccess()` callback | Not implemented (use `WorkWithCallback`) | None |
| `onFailureRetry()` callback | Not implemented | None |
| `onFailureRaise()` callback | Not implemented | None |
| `getRetryDelay()` exponential backoff | Work items control delay via `WorkOutcome::Retry { delay }` | Partial |
| `getRetryETA()` | Not implemented | None |

### Work class - hierarchical work (`lib.rs`)

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
| `onRun()` with round-robin dispatch | (not implemented) | None |
| `onAbort()` with child abort | (not implemented) | None |
| `onReset()` with child cleanup | (not implemented) | None |
| `doWork()` pure virtual | (not implemented) | None |
| `doReset()` virtual | (not implemented) | None |
| `checkChildrenStatus()` | (not implemented) | None |
| `yieldNextRunningChild()` | (not implemented) | None |
| `WorkUtils::getWorkStatus()` | (not implemented) | None |
| `WorkUtils::allSuccessful()` | (not implemented) | None |
| `WorkUtils::anyFailed()` | (not implemented) | None |
| `WorkUtils::anyRunning()` | (not implemented) | None |

### WorkScheduler (`lib.rs`)

Corresponds to: `WorkScheduler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkScheduler()` constructor | `WorkScheduler::new()` | Full |
| `create()` factory | `WorkScheduler::new()` | Full |
| `executeWork<T>()` blocking run | `run_until_done()` | Full |
| `scheduleWork<T>()` non-blocking | `add_work()` + `run_until_done()` | Full |
| `shutdown()` | `cancel_all()` | Full |
| `doWork()` scheduler loop | `run_until_done_with_cancel()` | Full |
| `scheduleOne()` IO posting | Tokio task spawning | Full |

### WorkSequence (`lib.rs`)

Corresponds to: `WorkSequence.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkSequence()` constructor | `WorkSequence::new()` | Full |
| `onRun()` sequential dispatch | `push()` creates dependency chain | Full |
| `onAbort()` abort current | Handled by scheduler cancellation | Full |
| `onReset()` | Not needed (scheduler handles retries) | Full |
| `getStatus()` | Not implemented (scheduler has snapshot) | Partial |
| `shutdown()` | Handled by scheduler cancel | Full |
| `stopAtFirstFailure` flag | Scheduler blocks dependents on failure | Full |

### BatchWork (not implemented)

Corresponds to: `BatchWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BatchWork()` constructor | (not implemented) | None |
| `getNumWorksInBatch()` | (not implemented) | None |
| `doReset()` | (not implemented) | None |
| `doWork()` batch management | (not implemented) | None |
| `hasNext()` pure virtual | (not implemented) | None |
| `yieldMoreWork()` pure virtual | (not implemented) | None |
| `resetIter()` pure virtual | (not implemented) | None |
| `addMoreWorkIfNeeded()` | (not implemented) | None |

### ConditionalWork (not implemented)

Corresponds to: `ConditionalWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ConditionalWork()` constructor | (not implemented) | None |
| `shutdown()` | (not implemented) | None |
| `getStatus()` | (not implemented) | None |
| `onRun()` condition polling | (not implemented) | None |
| `onAbort()` | (not implemented) | None |
| `onReset()` | (not implemented) | None |

### WorkWithCallback (`lib.rs`)

Corresponds to: `WorkWithCallback.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WorkWithCallback()` constructor | `WorkWithCallback::new()` | Full |
| `onRun()` callback execution | `Work::run()` delegates + callback | Full |
| `onAbort()` | Handled by cancellation token | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `VirtualClock` / `VirtualTimer` integration | Rust uses Tokio async timers instead |
| `postOnMainThread` scheduling | Rust uses Tokio task spawning |
| `shouldYield()` cooperative yielding | Rust async yields naturally at `.await` points |
| `wakeUp()` / `wakeSelfUpCallback()` / `setupWaitingCallback()` | Async/await eliminates explicit wake-up; waiting is implicit via `.await` |
| `onReset()` lifecycle hook | Work items manage their own state across retries; Rust ownership model makes explicit reset unnecessary |
| `TRIGGER_PERIOD` constant | Async event-driven scheduler does not need polling intervals |
| `mApp` application context in work | Rust work receives minimal `WorkContext`; app state passed via closures |
| Tracy profiling markers (`ZoneScoped`) | Not needed; can be added via `tracing` if required |
| `CLOG_*` logging macros | Rust uses `tracing` crate equivalently |
| `NonMovableOrCopyable` base | Rust ownership system enforces this naturally |
| `enable_shared_from_this` | Not needed; Rust ownership model avoids shared_ptr patterns |
| `ALLOWED_TRANSITIONS` / `assertValidTransition()` | Simpler state model makes exhaustive validation unnecessary |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `Work` class (hierarchical parent-child) | Medium | Flat DAG covers current use cases; may be needed for complex work trees |
| `BatchWork` class | Medium | Iterator-based parallel batch generation not yet needed |
| `ConditionalWork` class | Medium | Condition-gated work execution not yet needed |
| `onSuccess()` lifecycle hook | Low | Can use `WorkWithCallback` instead |
| `onFailureRetry()` lifecycle hook | Low | Work items handle retry logic in `run()` |
| `onFailureRaise()` lifecycle hook | Low | Error info captured in `WorkOutcome::Failed` |
| `getRetryETA()` | Low | No UI/status display currently needs this |
| `getStatus()` formatted strings | Low | `WorkSnapshot` provides equivalent data |
| `RETRY_NEVER` / `RETRY_ONCE` / `RETRY_A_FEW` / `RETRY_A_LOT` constants | Low | Callers pass numeric retry counts directly |

## Architectural Differences

1. **Execution Model**
   - **stellar-core**: Cooperative single-threaded execution via `crankWork()` called from the main IO loop. Work items yield by returning `WORK_WAITING` and resume via timer-based `wakeUp()`.
   - **Rust**: True async execution via Tokio tasks. Work items are spawned as independent tasks up to `max_concurrency`. Waiting is implicit via `.await`.
   - **Rationale**: Rust's async/await model eliminates the need for explicit WAITING/RETRYING states and timer-based wake-up, resulting in a simpler state machine.

2. **Work Hierarchy vs. Flat DAG**
   - **stellar-core**: Tree-structured work with `Work` as a parent managing `mChildren` list. Round-robin dispatch of children via `yieldNextRunningChild()`. `WorkSequence` and `BatchWork` are `BasicWork` subclasses.
   - **Rust**: Flat DAG with explicit dependency edges. All work items registered at the scheduler level. `WorkSequence` is a helper that adds dependency edges rather than a work item itself.
   - **Rationale**: A flat DAG is simpler to reason about and sufficient for the current catchup/history use cases. The hierarchical model provides no additional benefit in an async context where concurrency is managed by the runtime.

3. **Ownership During Execution**
   - **stellar-core**: Uses `shared_ptr` and `weak_ptr` for work items. Parent holds `shared_ptr` to children; callbacks capture `weak_ptr` to self.
   - **Rust**: Work items are moved into the scheduler. During execution, the real work is temporarily swapped out and replaced by an `EmptyWork` placeholder. Work is moved back after completion.
   - **Rationale**: Satisfies Rust's ownership rules without `Arc<Mutex<>>` overhead. Allows stateful work items to be retried without cloning.

4. **Retry Strategy**
   - **stellar-core**: Built-in exponential backoff via `getRetryDelay()` using `exponentialBackoff()`. Retry count managed by `BasicWork` with max retries.
   - **Rust**: Work items specify retry delay in `WorkOutcome::Retry { delay }`. Scheduler uses `config.retry_delay` as default. Work items control their own backoff.
   - **Rationale**: Gives work items more control over retry timing. Exponential backoff can be implemented per-work-item if needed.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| BasicWork | 1 TEST_CASE / 8 SECTION | 0 #[test] | Covered indirectly via scheduler tests |
| Work with children | 1 TEST_CASE / 4 SECTION | 0 #[test] | Hierarchical work not implemented |
| Work scheduling | 2 TEST_CASE / 4 SECTION | 2 #[tokio::test] | Dependency and retry tests |
| RunCommandWork | 1 TEST_CASE / 4 SECTION | 0 #[test] | RunCommandWork is in a different crate |
| WorkSequence | 1 TEST_CASE / 5 SECTION | 1 #[tokio::test] | Basic ordering test |
| BatchWork | 1 TEST_CASE / 2 SECTION | 0 #[test] | BatchWork not implemented |
| ConditionalWork | 1 TEST_CASE / 5 SECTION | 0 #[test] | ConditionalWork not implemented |
| WorkWithCallback | (tested inline) | 1 #[tokio::test] | Callback invocation test |
| Cancellation | (tested via shutdown) | 1 #[tokio::test] | External cancellation token |
| Metrics/Snapshots | (no direct tests) | 1 #[tokio::test] | Scheduler introspection |

### Test Gaps

- **BasicWork state machine tests**: stellar-core has 8 SECTIONs covering individual state transitions, waiting, shutdown, and mid-flight work addition. Rust tests do not directly test state transitions.
- **Hierarchical work tests**: 4 SECTIONs covering parent-child success, child failure, abort propagation. Not applicable since hierarchical model is not implemented.
- **BatchWork tests**: 2 SECTIONs covering success and shutdown of batch work. Not applicable.
- **ConditionalWork tests**: 5 SECTIONs covering condition satisfaction, failure, shutdown, and reset. Not applicable.
- **Multi-level tree scheduling**: 4 SECTIONs testing scheduling fairness across tree structures. Rust has no equivalent complex scheduling tests.
- **Failure propagation tests**: stellar-core tests child failure causing parent abort. Rust tests dependency blocking but not complex failure cascading.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 28 |
| Gaps (None + Partial) | 44 |
| Intentional Omissions | 12 |
| **Parity** | **28 / (28 + 44) = 39%** |

Detailed breakdown:
- **Full (28)**: BasicWork mapping (12) + WorkScheduler (7) + WorkSequence (6) + WorkWithCallback (3)
- **Partial (4)**: `RETRY_*` constants, `getStatus()` (BasicWork), `getRetryDelay()`, `getStatus()` (WorkSequence)
- **None (40)**: Work class hierarchy (22) + BatchWork (8) + ConditionalWork (6) + BasicWork lifecycle hooks (3: onSuccess, onFailureRetry, onFailureRaise) + `getRetryETA()` (1)
- **Intentional Omissions (12)**: Items excluded from the component tables entirely -- async-model replacements (6: VirtualClock/Timer, postOnMainThread, shouldYield, wakeUp/wakeSelfUpCallback/setupWaitingCallback, onReset, TRIGGER_PERIOD) + Rust-model replacements (2: NonMovableOrCopyable, enable_shared_from_this) + Logging/profiling (2: Tracy, CLOG) + App context (1: mApp) + State validation (1: ALLOWED_TRANSITIONS/assertValidTransition)
