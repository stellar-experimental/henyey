# Henyey Herder Crate — Specification Adherence Evaluation

**Evaluated against:** stellar-core v26.0.1 herder implementation (C++ reference)
**stellar-core pin:** v27.0.0 — evaluation body reflects the v26.0.1 baseline; Protocol 27 delta re-eval pending.
**Crate:** `crates/herder/` (henyey-herder)
**SCP core library:** `crates/scp/` (henyey-scp) — 95% parity (see `crates/scp/PARITY_STATUS.md`)
**Parity:** see `crates/herder/PARITY_STATUS.md` for current function-level status
**Date:** 2026-05-10

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Evaluation Methodology](#2-evaluation-methodology)
3. [Section-by-Section Evaluation](#3-section-by-section-evaluation)
   - [§1 SCP Driver Callbacks](#31-scp-driver-callbacks)
   - [§2 Value Validation & Nomination](#32-value-validation--nomination)
   - [§3 Envelope Reception & Processing](#33-envelope-reception--processing)
   - [§4 Transaction Set Construction](#34-transaction-set-construction)
   - [§5 Transaction Set Validation](#35-transaction-set-validation)
   - [§6 Transaction Queue (Mempool)](#36-transaction-queue-mempool)
   - [§7 Transaction Broadcasting](#37-transaction-broadcasting)
   - [§8 Surge Pricing & Lane Management](#38-surge-pricing--lane-management)
   - [§9 Parallel Soroban Phases](#39-parallel-soroban-phases)
   - [§10 Ledger Close & Externalization](#310-ledger-close--externalization)
   - [§11 Upgrades](#311-upgrades)
   - [§12 SCP State Persistence](#312-scp-state-persistence)
   - [§13 Herder State Machine & Sync Recovery](#313-herder-state-machine--sync-recovery)
   - [§14 Quorum Tracking & Intersection](#314-quorum-tracking--intersection)
   - [§15 Timer Management](#315-timer-management)
   - [§16 Drift Tracking](#316-drift-tracking)
   - [§17 Metrics & Observability](#317-metrics--observability)
4. [Gap Summary](#4-gap-summary)
5. [Risk Assessment](#5-risk-assessment)
6. [Recommendations](#6-recommendations)
7. [v26.0.1 Delta](#7-v2601-delta)

---

## 1. Executive Summary

The henyey herder crate coordinates SCP consensus on top of the core SCP library (`henyey-scp`, which is at **95% parity** with stellar-core's SCP implementation — see `crates/scp/PARITY_STATUS.md`). The herder is responsible for driving nomination, validating values, managing the transaction mempool, constructing and validating transaction sets, handling upgrades, persisting SCP state, and coordinating ledger closes.

The herder crate is at **79% function-level parity** (see `crates/herder/PARITY_STATUS.md` for current counts). However, function-level parity does not capture the full picture — many behaviors are implemented with different architectural patterns (e.g., unified `TransactionQueue` vs C++ inheritance hierarchy, actor-model `TimerManager` vs `VirtualTimer` per slot).

### Overall Adherence Rating

| Category | Rating | Notes |
|----------|--------|-------|
| **SCP Driver Callbacks** | **High** | `validateValue`, `combineCandidates`, `computeTimeout`, `extractValidValue` all implemented; missing `getNodeWeight` (P22+ leader election) |
| **Value Validation & Nomination** | **High** | Signature verification, close time validation, upgrade validation present; missing `ctValidityOffset()` clock drift adjustment |
| **Envelope Reception** | **High** | Close time range check, signature verification, dedup, dependency fetching all implemented; missing `isNewerNominationOrBallotSt()` dedup optimization |
| **Transaction Set Construction** | **High** | `build_generalized_tx_set()` covers core path; missing `TxSetPhaseFrame` abstraction and some utility methods |
| **Transaction Set Validation** | **Medium** | Inline validation present but no `ApplicableTxSetFrame::checkValid()` equivalent with LRU caching; missing `getTxBaseFee()`, `getPhase()` accessors |
| **Transaction Queue** | **High** | Unified queue with lane-based eviction, replace-by-fee (10x), Soroban resource checking; missing arb tx damping and `sourceAccountPending()` |
| **Transaction Broadcasting** | **Medium** | Surge-pricing-ordered broadcasting present; missing arb tx damping via Tarjan SCC |
| **Surge Pricing** | **Full** | Multi-lane priority queues with BTreeSet ordering, deterministic tie-breaking |
| **Parallel Soroban Phases** | **Full** | Conflict detection, greedy stage assignment, bin packing implemented |
| **Ledger Close** | **High** | `valueExternalized()`, queue cleanup, tracking update all present; missing slow SCP info dump and quorum intersection check post-close |
| **Upgrades** | **High** | 7 upgrade types, creation, validation for nomination; missing `removeUpgrades()` post-externalization cleanup and JSON getters |
| **SCP State Persistence** | **Full** | Save/restore SCP envelopes + tx sets + quorum sets via SQLite |
| **Herder State Machine** | **Full** | `Booting` → `Syncing` → `Tracking` with out-of-sync recovery |
| **Quorum Tracking** | **Medium** | Basic transitive quorum tracking present; missing `checkAndMaybeReanalyzeQuorumMap()` background intersection analysis |
| **Timer Management** | **Full** | Actor-model `TimerManager` with tokio channels (architectural departure, functionally equivalent) |
| **Drift Tracking** | **Full** | Sliding window of 120 ledgers with 10s threshold |
| **Metrics & Observability** | **Low** | 7 ballot phase callbacks, SCP execution metrics, and cost tracking all missing |

**Estimated behavioral coverage: ~85%** of consensus-critical behavior is implemented. The remaining gaps fall into three categories: (1) 5 consensus-critical gaps requiring attention, (2) ~15 operational gaps that affect robustness but not correctness, and (3) ~15 metrics/observability gaps that do not affect consensus.

---

## 2. Evaluation Methodology

This evaluation compares the henyey herder implementation against the stellar-core v26.0.1 reference source code. Every C++ source file in `stellar-core/src/herder/` was read in full and compared against the corresponding Rust implementation.

Each behavior is assessed on three dimensions:

1. **Structural completeness**: Are the required data structures, abstractions, and state machines present?
2. **Behavioral correctness**: Do the implementations follow the same algorithms, state transitions, and edge case handling?
3. **Constant fidelity**: Do hardcoded values, thresholds, and timeouts match?

Ratings per requirement:

| Symbol | Meaning |
|--------|---------|
| ✅ | Fully implemented and matches stellar-core |
| ⚠️ | Partially implemented or minor deviation |
| ❌ | Not implemented |
| ➖ | Not applicable (intentional architectural departure) |

Source file references use the format `file.rs:line`.

---

## 3. Section-by-Section Evaluation

### 3.1 SCP Driver Callbacks

**Source files:** `scp_driver.rs`, `herder.rs`
**Reference:** `HerderSCPDriver.cpp` (1,463 lines), `HerderSCPDriver.h` (277 lines)

The SCP driver is the bridge between the SCP state machine and the herder. It implements callbacks that the SCP core calls during consensus rounds.

#### Core Callbacks

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `validateValue()` — full value validation | ✅ | `scp_driver.rs`: Deserializes XDR, checks `STELLAR_VALUE_SIGNED`, verifies signature, validates close time, checks tx set validity, validates upgrades |
| `combineCandidates()` — merge nominated values | ✅ | `scp_driver.rs`: Selects largest tx set by size/fees/hash, merges upgrades taking max of each type |
| `computeTimeout()` — ballot timeout calculation | ✅ | `scp_driver.rs`: Linear timeout starting at 1s with 1s increment |
| `computeTimeout()` — P23+ network config values | ⚠️ | Uses hardcoded linear formula; stellar-core P23+ reads `SCP_BALLOT_BASE_TIMEOUT` and `SCP_BALLOT_MAX_TIMEOUT` from network config |
| `computeTimeout()` — 30-minute cap | ✅ | Maximum timeout capped at `MAX_TIMEOUT_SECONDS` (1800s) |
| `extractValidValue()` — strip invalid upgrades | ✅ | `scp_driver.rs`: Strips invalid upgrade steps from otherwise valid values |
| `valueExternalized()` — handle externalized value | ✅ | `scp_driver.rs`: Cancels timers ≤ slot, stops nomination, updates tracking |
| `nominate()` — submit value for nomination | ✅ | `scp_driver.rs`: Builds `StellarValue`, signs, nominates via SCP |
| `emitEnvelope()` — broadcast SCP envelope | ✅ | `scp_driver.rs`: Signs and broadcasts via overlay |
| `getNodeWeight()` — P22+ application-specific weight | ❌ | Not implemented; uses default equal weighting. Stellar-core uses validator quality levels and home domain sizes for leader election |
| `timerCallbackWrapper()` — reschedule for future slots | ✅ | `scp_driver.rs`: Reschedules timers when tracking wrong slot |

#### Envelope Handling

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `wrapEnvelope()` — add NodeID for logging | ❌ | Not implemented; stellar-core wraps envelopes with cached NodeID for O(1) lookup |
| `wrapStellarValue()` / `wrapValue()` | ❌ | Not implemented; stellar-core caches deserialized `StellarValue` to avoid repeated XDR parsing |
| Envelope signing with Ed25519 | ✅ | `scp_driver.rs`: Signs envelopes using node's secret key |
| Envelope signature verification | ✅ | `scp_driver.rs`: Verifies signatures on received envelopes |

#### Tx Set Validity Caching

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `checkAndCacheTxSetValid()` — LRU cache for validation results | ⚠️ | LRU cache exists in `scp_driver.rs` but stellar-core has a separate 1000-entry `mTxSetValidCache` with specific close-time-offset keying |
| `cacheValidTxSet()` — cache nominated tx set | ❌ | Not implemented; stellar-core pre-caches the tx set nominated by this node to skip re-validation |

**Assessment: High adherence on core callbacks.** The fundamental SCP driver callbacks (`validateValue`, `combineCandidates`, `computeTimeout`, `extractValidValue`, `valueExternalized`) are all correctly implemented. The two significant gaps are: (1) `getNodeWeight()` which affects P22+ leader election, and (2) the `wrapEnvelope()`/`wrapValue()` caching optimization which affects performance but not correctness.

---

### 3.2 Value Validation & Nomination

**Source files:** `scp_driver.rs`, `herder.rs`
**Reference:** `HerderSCPDriver.cpp:validateValue()`, `HerderImpl.cpp:triggerNextLedger()`

#### Value Validation Pipeline

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Deserialize `StellarValue` from XDR | ✅ | `scp_driver.rs`: XDR deserialization with error handling |
| Check `ext` discriminant is `STELLAR_VALUE_SIGNED` | ✅ | `scp_driver.rs`: Rejects `STELLAR_VALUE_BASIC` |
| Verify `lcValueSignature` signature | ✅ | `scp_driver.rs`: Ed25519 verification over `(networkID ∥ ENVELOPE_TYPE_SCP_VALUE ∥ stellarValue)` |
| Validate close time not too far in future | ✅ | `scp_driver.rs`: Close time checked against `upperBoundCloseTimeOffset` |
| Validate close time > last close time | ✅ | `scp_driver.rs`: Ensures monotonic close time |
| Validate tx set hash exists / can be fetched | ✅ | `scp_driver.rs`: Tx set fetched and validated |
| Validate upgrade steps are ordered by type | ✅ | `scp_driver.rs`: Upgrade ordering enforced |
| Validate each upgrade step individually | ✅ | `scp_driver.rs`: Per-upgrade validation via `upgrades.rs` |
| `ctValidityOffset()` — clock drift adjustment | ❌ | Not implemented; stellar-core adjusts close time offset based on local drift estimate for more accurate validation |

#### Nomination Trigger

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `setupTriggerNextLedger()` — schedule nomination | ✅ | `herder.rs`: Trigger scheduled based on expected close time |
| Calculate trigger time from last ballot start + expected close time | ✅ | `herder.rs`: Uses tracking data to compute trigger time |
| Adjust for clock drift via `ctValidityOffset()` | ❌ | Not implemented (same as above) |
| `triggerNextLedger()` — build and nominate value | ✅ | `herder.rs`: Builds tx set from queue, creates upgrades, nominates |
| Build `StellarValue` with signed extension | ✅ | `scp_driver.rs`: Signs value with `lcValueSignature` |

**Assessment: High adherence.** The validation pipeline is complete and correctly ordered. The missing `ctValidityOffset()` means henyey does not adjust close time validation bounds based on observed clock drift, which could cause unnecessary value rejections on nodes with slightly drifted clocks.

---

### 3.3 Envelope Reception & Processing

**Source files:** `herder.rs`, `fetching_envelopes.rs`, `pending.rs`
**Reference:** `HerderImpl.cpp:recvSCPEnvelope()`, `PendingEnvelopes.cpp` (996 lines)

#### Envelope Reception Pipeline

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Close time range check on received envelope | ✅ | `herder.rs`: Checks close time against tracking state and drift bounds |
| Slot number range validation (not too old/far future) | ✅ | `herder.rs`: Rejects envelopes for slots outside acceptable range |
| Signature verification on received envelope | ✅ | `herder.rs`: Ed25519 signature verification |
| Self-envelope detection | ✅ | `herder.rs`: Skips envelopes from own node |
| Dedup check — reject already-seen envelopes | ✅ | `pending.rs`: Hash-based dedup of processed envelopes |
| `isNewerNominationOrBallotSt()` — reject stale envelopes | ❌ | Not implemented; stellar-core checks whether an envelope is newer than the current nomination/ballot statement for the same node+slot before processing |
| Dependency fetching (tx sets, quorum sets) | ✅ | `fetching_envelopes.rs`: Tracks missing dependencies, retries with peer rotation |
| Ready queue — process when dependencies satisfied | ✅ | `fetching_envelopes.rs`: Envelopes queued until all dependencies available |
| SCP queue processing with slot prioritization | ✅ | `herder.rs`: Processes envelopes in slot order |

#### PendingEnvelopes Subsystem

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Quorum membership check before processing | ⚠️ | Basic quorum check present; stellar-core has a more sophisticated transitive quorum filter |
| `STELLAR_VALUE_SIGNED` check on received values | ✅ | Verified during envelope validation |
| Quorum set cache (10K entries, LRU) | ⚠️ | Quorum sets cached but may not match stellar-core's exact 10K LRU with `weak_ptr` tracking |
| Tx set cache (10K entries, LRU) | ⚠️ | Tx sets cached but may not match stellar-core's exact 10K LRU |
| `envelopeReady()` — broadcast + wrap + queue | ✅ | Ready envelopes are broadcast to overlay and queued for SCP processing |
| `rebuildQuorumTrackerState()` — rebuild transitive quorum from latest SCP messages | ❌ | Not implemented; stellar-core rebuilds transitive quorum from latest SCP messages + DB fallback |
| `recordReceivedCost()` — per-validator cost tracking | ❌ | Not implemented |
| `reportCostOutliersForSlot()` — K-means clustering outlier detection | ❌ | Not implemented; stellar-core uses 3-cluster K-means with 10x outlier ratio to identify misbehaving validators |
| `recomputeKeysToFilter()` — ban overlay keys for outlier validators | ❌ | Not implemented |

#### Dependency Fetching

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Fetch tx set from peer that sent envelope | ✅ | `fetching_envelopes.rs`: Requests from originating peer first |
| Fetch quorum set from peer that sent envelope | ✅ | `fetching_envelopes.rs`: Same pattern |
| Retry with peer rotation on timeout | ✅ | `fetching_envelopes.rs`: Rotates through available peers |
| Timeout and discard after max retries | ✅ | `fetching_envelopes.rs`: Configurable retry limits |
| `DONT_HAVE` handling — try next peer | ✅ | `fetching_envelopes.rs`: Handles `DONT_HAVE` response |

**Assessment: High adherence on core pipeline, medium on optimization subsystems.** The fundamental envelope reception, validation, dependency fetching, and SCP queuing are all correctly implemented. The gaps are in optimization/protection layers: stale envelope rejection (`isNewerNominationOrBallotSt`), per-validator cost tracking, and K-means outlier detection. The cost tracking and outlier detection are protection against misbehaving validators and don't affect consensus correctness.

---

### 3.4 Transaction Set Construction

**Source files:** `tx_queue/selection.rs`, `tx_queue/tx_set.rs`, `surge_pricing.rs`, `parallel_tx_set_builder.rs`
**Reference:** `TxSetFrame.cpp:makeTxSetFromTransactions()` (lines 1-400)

#### Building Transaction Sets for Nomination

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Group transactions by account (account queues) | ✅ | `selection.rs`: Groups by source account |
| Split into classic and Soroban phases | ✅ | `selection.rs`: Separate phase construction |
| Trim invalid transactions before building | ✅ | `tx_set_utils.rs`: `get_invalid_tx_list()` + `trim_invalid()` |
| Apply surge pricing to each phase | ✅ | `selection.rs`: Applies surge pricing via `surge_pricing.rs` |
| Roundtrip through XDR to verify determinism | ⚠️ | Tx set serialized to XDR but may not do the exact roundtrip-then-re-validate pattern stellar-core uses |
| `TxSetPhaseFrame` abstraction (sequential vs parallel) | ❌ | No `TxSetPhaseFrame` type; phases are handled as flat transaction lists. Stellar-core has a rich `TxSetPhaseFrame` with sequential/parallel variants, `checkValid()`, `sortedForApply()`, `toXDR()`, iterator support |
| `buildAccountTxQueues()` — priority-ordered account grouping | ⚠️ | Account grouping present but may not match stellar-core's exact priority ordering within account queues |
| `ApplicableTxSetFrame` abstraction — validated tx set | ❌ | No equivalent type. Stellar-core's `ApplicableTxSetFrame` wraps validated phases with `getContentsHash()`, `getTxBaseFee()`, `getPhase()`, `getPhasesInApplyOrder()`, `checkValid()`, `summary()` |
| Content hash computation | ✅ | `tx_set.rs`: Content hash computed from XDR |
| Legacy vs generalized tx set detection | ✅ | `tx_set.rs`: Handles both `TransactionSet` and `GeneralizedTransactionSet` |

#### Generalized Tx Set XDR Construction

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Generalized tx set with per-component base fees | ✅ | `selection.rs`: Components with discounted/non-discounted fee groups |
| Sequential phase XDR (v0) | ✅ | `tx_set.rs`: Sequential phase serialization |
| Parallel phase XDR (v1) | ✅ | `tx_set.rs`: Parallel phase with stages/clusters |
| Previous ledger hash in tx set | ✅ | `tx_set.rs`: Previous ledger hash included |

**Assessment: High adherence on the core construction path.** Transaction sets are correctly built with account grouping, phase splitting, surge pricing, and XDR serialization. The structural gap is the absence of the `TxSetPhaseFrame` and `ApplicableTxSetFrame` abstractions, which stellar-core uses to encapsulate validation state, per-transaction base fees, and apply-order sorting. In henyey, these behaviors are handled inline but without the type-safety guarantees of the C++ abstraction hierarchy.

---

### 3.5 Transaction Set Validation

**Source files:** `tx_queue/tx_set.rs`, `tx_set_utils.rs`, `scp_driver.rs`
**Reference:** `TxSetFrame.cpp:prepareForApply()`, `TxSetPhaseFrame::checkValid()`, `ApplicableTxSetFrame::checkValidInternal()`

#### Validation on Receipt (from SCP envelope)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Previous ledger hash matches LCL | ✅ | `tx_set.rs`: Hash comparison |
| Generalized vs legacy format matches protocol version | ✅ | `tx_set.rs`: Format check against protocol version |
| Phase count validation (1 for legacy, 2 for generalized) | ✅ | `tx_set.rs`: Phase count validated |
| No duplicate source accounts across phases | ✅ | `tx_set.rs`: Source account uniqueness enforced |
| Per-phase validation (classic vs Soroban) | ✅ | `tx_set.rs`: Phase-specific validation |
| Fee map validation | ⚠️ | Fee validation present but may not fully match stellar-core's `checkFeeMap()` which validates component structure |
| Classic phase: size ≤ `maxTxSetSize` | ✅ | Validated against header limits |
| Classic phase: not parallel | ✅ | Classic phase always sequential |
| Soroban phase: total resources ≤ ledger limits | ✅ | Resource validation against Soroban config |
| Soroban phase: parallel iff protocol ≥ `PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION` | ✅ | Protocol version check for parallel support |
| Parallel Soroban: cluster count ≤ `ledgerMaxDependentTxClusters` | ✅ | Cluster limit validated |
| Parallel Soroban: sequential instruction limit (sum of max-per-stage) | ✅ | `parallel_tx_set_builder.rs`: Instruction accounting |
| Parallel Soroban: no read-write conflicts between clusters in a stage | ✅ | `parallel_tx_set_builder.rs`: Conflict detection |
| Per-transaction `checkValid()` for unvalidated tx sets | ⚠️ | Individual tx validation present but done differently than stellar-core's `txsAreValid()` which uses `LedgerSnapshot` at LCL+1 |
| `checkAndCacheTxSetValid()` — LRU cached validation results | ⚠️ | Cache exists but may differ in key structure (close-time offset keying) |

#### Apply-Order Sorting

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `sortedForApply()` — deterministic apply order | ✅ | `tx_set.rs`: Apply-order sorting |
| Sequential: sort by account, then by hash with tx set hash seed | ✅ | Hash-based deterministic ordering |
| Parallel: sort within clusters, preserve stage/cluster structure | ✅ | `parallel_tx_set_builder.rs`: Maintains structure |
| `sortParallelTxsInHashOrder()` — parallel phase sorting | ✅ | Implemented |

**Assessment: High adherence.** All critical validation checks are present including parallel Soroban conflict detection and resource limit enforcement. The gap is primarily structural — the absence of `ApplicableTxSetFrame` means validation results aren't encapsulated in a type that guarantees a tx set has been validated before use.

---

### 3.6 Transaction Queue (Mempool)

**Source files:** `tx_queue/mod.rs` (4,120+ lines)
**Reference:** `TransactionQueue.cpp` (1,425 lines), `TxQueueLimiter.cpp` (315 lines)

#### Transaction Addition (`tryAdd`)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Ban check — reject banned transactions | ✅ | `tx_queue/mod.rs`: Ban deque checked |
| Filter check — reject if node is filtering | ⚠️ | Basic filtering present; stellar-core's `recomputeKeysToFilter()` for outlier validators not implemented |
| Soroban footprint filter | ⚠️ | Soroban resource checking present; stellar-core has specific footprint overlap filtering |
| Fee validation — minimum fee check | ✅ | `tx_queue/mod.rs`: Fee validated against minimums |
| Account state check — sequence number validation | ✅ | `tx_queue/mod.rs`: Sequence number validated |
| Soroban resource check — resources within limits | ✅ | `tx_queue/mod.rs`: Soroban resources validated |
| Fee-bump sequence match | ✅ | `tx_queue/mod.rs`: Fee-bump must target same account |
| Limiter eviction — evict lower-fee txs to make room | ✅ | `tx_queue_limiter.rs`: Resource-based eviction |
| `checkValid()` — full transaction validation | ✅ | Full validation performed |
| Fee balance check — account can pay fee | ✅ | Balance validation |
| Replace-by-fee — 10x fee multiplier | ✅ | `tx_queue/mod.rs`: `canReplaceByFee` with 10x multiplier |
| Replace-by-fee — 128-bit arithmetic for overflow safety | ✅ | Uses `u128` arithmetic to prevent overflow |
| Memo validation (pre-P25) | ❌ | Not implemented; stellar-core validates memo deduplication before P25 |
| Extra signer validation | ✅ | `tx_queue/mod.rs`: Extra signers checked |
| One-tx-per-account model | ✅ | `tx_queue/mod.rs`: One active tx per source account |

#### Transaction Removal & Aging

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `removeApplied()` — remove after ledger close | ✅ | `tx_queue/mod.rs`: Applied txs removed |
| `shift()` — age increment per ledger | ✅ | `tx_queue/mod.rs`: Age tracking |
| Auto-ban at `mPendingDepth` (4 ledgers) | ✅ | `tx_queue/mod.rs`: Automatic banning after timeout |
| `TRANSACTION_QUEUE_TIMEOUT_LEDGERS` = 4 | ✅ | Matches stellar-core constant |
| `TRANSACTION_QUEUE_BAN_LEDGERS` = 10 | ✅ | Matches stellar-core constant |
| `ban()` — add to ban deque | ✅ | `tx_queue/mod.rs`: Ban deque management |
| `isBanned()` check | ✅ | `tx_queue/mod.rs`: Ban check on add |
| `sourceAccountPending()` — check if account has pending tx | ❌ | Not implemented; stellar-core exposes this for other subsystems to query |

#### Arb Transaction Damping

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `allowTxBroadcast()` — arb tx damping | ❌ | Not implemented |
| Payment loop detection via Tarjan SCC on asset graph | ❌ | Not implemented; stellar-core builds a directed asset graph from payment/path-payment ops and detects cycles using Tarjan's SCC algorithm |
| Geometric distribution dampening for arb txs | ❌ | Not implemented; stellar-core uses `std::geometric_distribution` with p=0.1 to probabilistically suppress arb txs |
| Clear arb damping state on `shift()` | ❌ | Not part of shift logic |

#### Queue Rebuilding

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `resetAndRebuild()` — Soroban queue post-upgrade | ⚠️ | Queue clearing present but may not fully re-add existing txs as stellar-core does |
| `updateTransactionQueue()` — post-externalization cleanup | ✅ | `herder.rs`: Removes applied, shifts age, bans invalid |

**Assessment: High adherence on core mempool operations.** Transaction addition, validation, replace-by-fee, aging, and banning all match stellar-core. The notable missing feature is arb transaction damping, which stellar-core uses to suppress arbitrage transactions that create payment loops. This is a network-health feature rather than a consensus requirement — arb damping reduces unnecessary network load but doesn't affect consensus outcomes.

---

### 3.7 Transaction Broadcasting

**Source files:** `tx_broadcast.rs` (508 lines)
**Reference:** `TransactionQueue.cpp:broadcastSome()`

#### Classic Broadcasting

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Surge-pricing-ordered broadcasting | ✅ | `tx_broadcast.rs`: Ordered by fee priority |
| DEX lane limits during broadcast | ⚠️ | Lane awareness present; exact DEX lane limit enforcement may differ |
| Arb tx banning during broadcast | ❌ | Not implemented (depends on arb damping) |
| Carryover up to `MAX_OPS_PER_TX + 1` | ⚠️ | Carryover logic present but may not match exact limit |
| Flood queue with deterministic seed reset on `shift()` | ⚠️ | Broadcast queue present; seed reset behavior may differ |

#### Soroban Broadcasting

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Multi-resource limit enforcement | ✅ | `tx_broadcast.rs`: Resource-aware broadcasting |
| No arb damping for Soroban | ✅ | Soroban path doesn't have arb damping (correct — stellar-core doesn't either) |
| Carryover limited to max single tx resources | ⚠️ | Carryover present but exact limit may differ |

#### Flow Control Integration

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `visitTopTxs()` — top-N tx iteration for flood queue | ⚠️ | `tx_queue_limiter.rs`: Has `visitTopTxs` equivalent but may not be fully integrated |
| Separate flood queue from main queue | ⚠️ | `tx_queue_limiter.rs`: Has flood queue concept but integration unclear |
| `getMaxQueueSizeSorobanOps()` — dynamic Soroban queue sizing | ❌ | Not implemented; stellar-core dynamically sizes the Soroban flood queue based on network config |

**Assessment: Medium adherence.** The core broadcasting path with surge-pricing ordering works correctly. The gaps are in arb tx suppression during broadcast and some fine-grained queue sizing for Soroban operations.

---

### 3.8 Surge Pricing & Lane Management

**Source files:** `surge_pricing.rs` (774 lines)
**Reference:** `TxSetFrame.cpp:applySurgePricing()`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Multi-lane priority queue | ✅ | `surge_pricing.rs`: `BTreeSet`-based multi-lane queues |
| Per-lane operation limits | ✅ | `surge_pricing.rs`: Lane-specific capacity limits |
| Global lane (lane 0) encompassing all txs | ✅ | `surge_pricing.rs`: Global lane always present |
| DEX operations lane | ✅ | `surge_pricing.rs`: DEX lane with separate limit |
| Deterministic tie-breaking by hash | ✅ | `surge_pricing.rs`: Hash-based tiebreaker |
| Per-lane base fee computation | ✅ | `surge_pricing.rs`: Base fee from eviction threshold |
| Discounted vs non-discounted fee components | ✅ | `selection.rs`: Separate fee components in generalized tx set |
| Eviction of lowest-fee transactions when over limit | ✅ | `surge_pricing.rs`: Eviction from lowest-fee end |

**Assessment: Full adherence.** The surge pricing implementation with multi-lane BTreeSet-based priority queues matches stellar-core's behavior including deterministic tie-breaking and per-lane base fee computation.

---

### 3.9 Parallel Soroban Phases

**Source files:** `parallel_tx_set_builder.rs` (1,013 lines)
**Reference:** `TxSetFrame.cpp` parallel phase construction + validation

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Conflict detection on Soroban footprints | ✅ | `parallel_tx_set_builder.rs`: Read-write conflict analysis |
| Greedy stage assignment | ✅ | `parallel_tx_set_builder.rs`: Greedy algorithm for stage allocation |
| Bin packing for cluster assignment | ✅ | `parallel_tx_set_builder.rs`: Bin packing optimization |
| Instruction limit per stage (max of clusters) | ✅ | `parallel_tx_set_builder.rs`: Per-stage instruction accounting |
| Cluster count limit per stage | ✅ | `parallel_tx_set_builder.rs`: `ledgerMaxDependentTxClusters` enforced |
| Custom `BitSet` for efficient footprint tracking | ✅ | `parallel_tx_set_builder.rs`: Custom `BitSet` implementation |
| Stage/cluster/transaction hierarchy | ✅ | `parallel_tx_set_builder.rs`: Three-level hierarchy |

**Assessment: Full adherence.** The parallel Soroban phase builder is comprehensively implemented with conflict detection, greedy stage assignment, and bin packing matching stellar-core's algorithm.

---

### 3.10 Ledger Close & Externalization

**Source files:** `herder.rs`, `scp_driver.rs`
**Reference:** `HerderImpl.cpp:valueExternalized()`, `HerderSCPDriver.cpp:valueExternalized()`

#### SCP Driver Externalization

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Cancel all timers ≤ externalized slot | ✅ | `scp_driver.rs`: Timer cancellation |
| Stop nomination for externalized slot | ✅ | `scp_driver.rs`: Nomination stopped |
| Update tracking state | ✅ | `herder.rs`: Tracking state updated |
| Record externalization metrics | ❌ | Not implemented; stellar-core records ballot/nomination counters |
| Notify `HerderImpl::valueExternalized()` | ✅ | `herder.rs`: Main externalization handler called |

#### Herder Externalization

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Record close time drift | ✅ | `drift_tracker.rs`: Drift recorded |
| Dump slow SCP info | ❌ | Not implemented; stellar-core dumps detailed SCP info when consensus was slow |
| Process externalized value (deserialize, extract tx set) | ✅ | `herder.rs`: Full deserialization and processing |
| Update transaction queue (remove applied, ban invalid, shift age) | ✅ | `herder.rs`: `updateTransactionQueue()` equivalent |
| Clean up old SCP slots | ✅ | `herder.rs`: Old slot cleanup |
| Check quorum intersection (async background) | ❌ | Not implemented; stellar-core triggers `checkAndMaybeReanalyzeQuorumMap()` after each externalization |
| Persist SCP state | ✅ | `persistence.rs`: SCP state saved to DB |
| `CONSENSUS_STUCK_TIMEOUT_SECONDS` = 35 | ✅ | Matches stellar-core constant |

**Assessment: High adherence.** The critical externalization path — timer cleanup, state update, transaction queue maintenance, persistence — is fully implemented. The gaps are in diagnostics (slow SCP info dump) and background quorum analysis, neither of which affects consensus correctness.

---

### 3.11 Upgrades

**Source files:** `upgrades.rs` (985 lines)
**Reference:** `Upgrades.cpp` (1,501 lines)

#### Upgrade Types

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `LEDGER_UPGRADE_VERSION` — protocol version upgrade | ✅ | `upgrades.rs`: Protocol version upgrade |
| `LEDGER_UPGRADE_BASE_FEE` — base fee upgrade | ✅ | `upgrades.rs`: Base fee upgrade |
| `LEDGER_UPGRADE_MAX_TX_SET_SIZE` — max tx set size | ✅ | `upgrades.rs`: Max tx set size upgrade |
| `LEDGER_UPGRADE_BASE_RESERVE` — base reserve upgrade | ✅ | `upgrades.rs`: Base reserve upgrade |
| `LEDGER_UPGRADE_FLAGS` — ledger flags upgrade | ✅ | `upgrades.rs`: Flags upgrade |
| `LEDGER_UPGRADE_CONFIG` — Soroban config upgrade | ✅ | `upgrades.rs`: Config upgrade support |
| `LEDGER_UPGRADE_MAX_SOROBAN_TX_SET_SIZE` — Soroban tx set size | ✅ | `upgrades.rs`: Soroban tx set size upgrade |

#### Upgrade Creation & Validation

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `createUpgradesFor()` — create upgrade proposals based on config | ✅ | `upgrades.rs`: Compares configured parameters vs LCL state |
| `isValidForNomination()` — check time window + parameter match | ✅ | `upgrades.rs`: Time window and parameter validation |
| `isValidForApply()` — per-type validation | ✅ | `upgrades.rs`: Version monotonic, fees non-zero, flags masked, etc. |
| Version upgrade must be monotonically increasing | ✅ | `upgrades.rs`: Monotonic check |
| Fees must be non-zero | ✅ | `upgrades.rs`: Non-zero fee validation |
| Flags must use valid mask | ✅ | `upgrades.rs`: Flag mask validation |
| Config upgrade: lookup from ledger, validate XDR (sorted, no duplicates, non-empty), check hash | ✅ | `upgrades.rs`: Config upgrade validation |
| Upgrade time window enforcement | ✅ | `upgrades.rs`: Only propose upgrades within configured time window |

#### Post-Externalization

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `removeUpgrades()` — clear matching parameters after externalization | ❌ | Not implemented; stellar-core removes upgrade parameters that have been applied to prevent re-proposing |
| 12-hour expiry safety for upgrade removal | ❌ | Not implemented (depends on `removeUpgrades`) |
| `setUpgrades()` — set upgrade parameters via admin | ❌ | Not implemented; stellar-core has runtime upgrade parameter configuration |
| `getUpgradesJson()` — query current upgrade settings | ❌ | Not implemented |

**Assessment: High adherence on upgrade creation and validation.** All 7 upgrade types are supported with correct creation and validation logic. The gap is in post-externalization cleanup (`removeUpgrades()`) — without this, a node might continue proposing upgrades that have already been applied, though the validation pipeline would reject duplicate upgrades from other nodes.

---

### 3.12 SCP State Persistence

**Source files:** `persistence.rs` (833 lines)
**Reference:** `HerderImpl.cpp:persistSCPState()`, `HerderImpl.cpp:restoreSCPState()`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Save SCP envelopes to DB | ✅ | `persistence.rs`: Envelopes serialized and stored |
| Save tx sets referenced by envelopes | ✅ | `persistence.rs`: Tx sets stored alongside envelopes |
| Save quorum sets referenced by envelopes | ✅ | `persistence.rs`: Quorum sets stored |
| Base64 encoding for DB storage | ✅ | `persistence.rs`: Base64 encoding used |
| Restore SCP state on startup | ✅ | `persistence.rs`: Full restore path |
| Replay persisted envelopes through SCP | ✅ | `persistence.rs`: Envelopes replayed |
| `HerderPersistence::getNodeQuorumSet()` | ❌ | Not implemented as separate query; stellar-core has `getNodeQuorumSet()` / `getQuorumSet()` DB queries |
| `saveSCPHistory()` — per-slot history for archiving | ❌ | Not implemented; stellar-core persists per-slot SCP messages for history archival (handled by henyey-history crate) |

**Assessment: Full adherence for consensus recovery purposes.** The save/restore cycle for SCP state is complete, allowing a node to resume consensus after restart. The missing `getNodeQuorumSet()` query is a convenience method, and `saveSCPHistory()` is an archival concern handled by a different crate.

---

### 3.13 Herder State Machine & Sync Recovery

**Source files:** `herder.rs`, `sync_recovery.rs` (584 lines), `state.rs` (118 lines)
**Reference:** `HerderImpl.cpp:stateChanged()`, `HerderImpl.cpp:outOfSyncRecovery()`

#### State Machine

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `HERDER_BOOTING_STATE` | ✅ | `state.rs`: `Booting` variant |
| `HERDER_SYNCING_STATE` | ✅ | `state.rs`: `Syncing` variant |
| `HERDER_TRACKING_STATE` | ✅ | `state.rs`: `Tracking` variant |
| `Booting` → `Syncing` on first SCP message | ✅ | `herder.rs`: State transition |
| `Syncing` → `Tracking` on externalization | ✅ | `herder.rs`: State transition |
| `Tracking` → `Syncing` on out-of-sync detection | ✅ | `herder.rs`: Fallback transition |
| `stateChanged()` — notify subsystems of state change | ❌ | Not implemented; stellar-core notifies LedgerManager, HistoryManager, etc. |

#### Out-of-Sync Recovery

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Detect out-of-sync via consensus stuck timeout | ✅ | `sync_recovery.rs`: Timeout-based detection |
| Purge old slots based on v-blocking set | ✅ | `sync_recovery.rs`: V-blocking-based slot purge |
| Rebroadcast own latest envelopes | ✅ | `sync_recovery.rs`: Rebroadcast logic |
| Request more SCP state from peers (`GET_SCP_STATE`) | ✅ | `sync_recovery.rs`: Requests additional state |
| `getMoreSCPState()` — multi-peer state request | ❌ | Not implemented as named method; behavior may be inline |
| Recovery rate limiting (don't spam requests) | ✅ | `sync_recovery.rs`: Rate-limited recovery |
| Heartbeat timer for tracking liveness | ✅ | `herder.rs`: Tracking heartbeat |

**Assessment: Full adherence on state machine and recovery.** The three-state machine with proper transitions and out-of-sync recovery is correctly implemented. The missing `stateChanged()` callback is a coordination concern — stellar-core uses it to notify other subsystems, while henyey may handle this differently through its async architecture.

---

### 3.14 Quorum Tracking & Intersection

**Source files:** `quorum_tracker.rs` (636 lines)
**Reference:** `HerderImpl.cpp:checkAndMaybeReanalyzeQuorumMap()`, `PendingEnvelopes.cpp:rebuildQuorumTrackerState()`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Track transitive quorum closure | ✅ | `quorum_tracker.rs`: Transitive quorum tracking |
| Update quorum map from SCP messages | ✅ | `quorum_tracker.rs`: Updated as envelopes processed |
| `rebuildQuorumTrackerState()` — full rebuild from latest messages + DB | ❌ | Not implemented; stellar-core periodically rebuilds the full transitive quorum map |
| `checkAndMaybeReanalyzeQuorumMap()` — background async quorum intersection analysis | ❌ | Not implemented; stellar-core runs quorum intersection checks in background with hash-based change detection |
| `forceRebuildQuorum()` — manual rebuild trigger | ❌ | Not implemented |
| Hash-based change detection for quorum map | ❌ | Not implemented; stellar-core only re-analyzes when quorum map hash changes |
| `resolveNodeID()` — NodeID to human-readable name | ❌ | Not implemented; convenience method |

**Assessment: Medium adherence.** Basic transitive quorum tracking is present, but the background quorum intersection analysis that stellar-core performs is entirely missing. This analysis detects quorum intersection failures and logs warnings — it's an operational safety feature. The QuorumIntersectionChecker itself is planned as a separate crate.

---

### 3.15 Timer Management

**Source files:** `timer_manager.rs` (628 lines)
**Reference:** `VirtualTimer` (stellar-core uses one `VirtualTimer` per SCP timer per slot)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Per-slot SCP timers | ✅ | `timer_manager.rs`: Timer per slot/type combination |
| Timer cancellation | ✅ | `timer_manager.rs`: Cancel by slot/type |
| Timer rescheduling | ✅ | `timer_manager.rs`: Reschedule support |
| Bulk cancel timers ≤ slot | ✅ | `timer_manager.rs`: Range cancellation |
| Actor-model with tokio channels | ➖ | Architectural departure from stellar-core's `VirtualClock` / `VirtualTimer`; functionally equivalent using tokio async runtime |

**Assessment: Full adherence (functionally equivalent).** The timer management is a clean architectural departure — using an actor-model with tokio channels instead of `VirtualTimer` per slot — but provides the same semantics.

---

### 3.16 Drift Tracking

**Source files:** `drift_tracker.rs` (500 lines)
**Reference:** `HerderImpl.cpp` close time drift tracking

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Sliding window of recent drift values | ✅ | `drift_tracker.rs`: Sliding window implementation |
| Window size = 120 ledgers | ✅ | `drift_tracker.rs`: 120-ledger window |
| Drift threshold = 10 seconds | ✅ | `drift_tracker.rs`: 10s threshold |
| Record drift per ledger close | ✅ | `drift_tracker.rs`: Per-close recording |
| Report excessive drift | ✅ | `drift_tracker.rs`: Drift reporting |

**Assessment: Full adherence.** Drift tracking matches stellar-core's implementation with the same window size and threshold.

---

### 3.17 Metrics & Observability

**Source files:** Various
**Reference:** `HerderSCPDriver.cpp` ballot phase callbacks, `HerderImpl.cpp:syncMetrics()`

#### Ballot Phase Callbacks

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `ballotDidHearFromQuorum()` | ❌ | Not implemented |
| `nominatingValue()` | ❌ | Not implemented |
| `updatedCandidateValue()` | ❌ | Not implemented |
| `startedBallotProtocol()` | ❌ | Not implemented |
| `acceptedBallotPrepared()` | ❌ | Not implemented |
| `confirmedBallotPrepared()` | ❌ | Not implemented |
| `acceptedCommit()` | ❌ | Not implemented |

#### SCP Execution Metrics

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `recordSCPExecutionMetrics()` — timing and counter metrics | ❌ | Not implemented |
| `recordSCPEvent()` — per-event recording | ❌ | Not implemented |
| `recordSCPExternalizeEvent()` — externalization-specific metrics | ❌ | Not implemented |
| `syncMetrics()` — periodic metric synchronization | ❌ | Not implemented |
| `getExternalizeLag()` / `getQsetLagInfo()` — latency diagnostics | ❌ | Not implemented |
| `getPrepareStart()` — ballot phase timing | ❌ | Not implemented |

#### PendingEnvelopes Cost Tracking

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `recordReceivedCost()` — per-validator cost tracking | ❌ | Not implemented |
| `reportCostOutliersForSlot()` — K-means outlier detection | ❌ | Not implemented |
| `getKnownQSetsCount()` / `getKnownTxSetCount()` | ❌ | Not implemented as named accessors |
| `getSCPQueuedEnvelopesCount()` | ❌ | Not implemented as named accessor |

**Assessment: Low adherence.** The metrics and observability layer is largely unimplemented. These 15+ missing callbacks and metrics do not affect consensus correctness but are important for production monitoring, debugging stuck consensus, and identifying misbehaving validators.

---

## 4. Gap Summary

### Consensus-Critical Gaps (5)

These gaps could affect consensus outcomes, leader election, or value validity under specific conditions.

| # | Gap | Severity | Impact |
|---|-----|----------|--------|
| G1 | `getNodeWeight()` — P22+ application-specific leader election | **High** | Without validator quality-based weighting, leader election is uniform rather than weighted by validator quality. Could affect which node's nominated value gets selected. Only matters for P22+ networks. |
| G2 | `isNewerNominationOrBallotSt()` — stale envelope rejection | **Medium** | Without this check, the node may re-process stale envelopes that have already been superseded. Wastes resources and could delay consensus in high-load scenarios. |
| G3 | `ctValidityOffset()` — clock drift adjustment for validation | **Medium** | Close time validation bounds are not adjusted for observed drift. Nodes with slightly drifted clocks may reject valid values or accept invalid ones. |
| G4 | `cacheValidTxSet()` — pre-cache self-nominated tx set | **Low** | Self-nominated tx sets are re-validated instead of being cached. Performance impact only; does not affect correctness. |
| G5 | `computeTimeout()` P23+ network config values | **Low** | Uses hardcoded timeout formula instead of reading `SCP_BALLOT_BASE_TIMEOUT` / `SCP_BALLOT_MAX_TIMEOUT` from network config. The hardcoded formula is the default; only diverges if the network has configured custom timeout values via upgrade. |

### Operational Gaps (15)

These gaps affect robustness, performance, or network health but not consensus correctness.

| # | Gap | Severity | Impact |
|---|-----|----------|--------|
| G6 | `allowTxBroadcast()` / arb tx damping | **Medium** | No suppression of arbitrage transactions. Higher network load from arb tx flooding. |
| G7 | `removeUpgrades()` — post-externalization cleanup | **Medium** | Applied upgrades continue to be proposed until manually removed. Other nodes' validation catches this, but it creates unnecessary traffic. |
| G8 | `checkAndMaybeReanalyzeQuorumMap()` — background quorum intersection analysis | **Medium** | No background detection of quorum intersection failures. Operational safety gap. |
| G9 | `rebuildQuorumTrackerState()` — full quorum rebuild | **Medium** | Transitive quorum map may become stale without periodic rebuilds. |
| G10 | `stateChanged()` — subsystem notification | **Low** | Other subsystems not notified of herder state changes. May affect coordination. |
| G11 | `sourceAccountPending()` — pending tx query | **Low** | Other subsystems cannot query whether an account has a pending transaction. |
| G12 | `setUpgrades()` / `getUpgradesJson()` — admin interface | **Low** | No runtime upgrade parameter management. Must be configured at startup. |
| G13 | `getMoreSCPState()` — multi-peer state request | **Low** | May already be implemented inline; named method missing. |
| G14 | `wrapEnvelope()` / `wrapValue()` — caching wrappers | **Low** | Performance optimization; repeated XDR parsing instead of cached deserialization. |
| G15 | `resetAndRebuild()` — full Soroban queue rebuild | **Low** | Soroban queue may not be fully rebuilt after upgrade. |
| G16 | `getMaxQueueSizeSorobanOps()` — dynamic queue sizing | **Low** | Soroban flood queue not dynamically sized from network config. |
| G17 | `TxSetPhaseFrame` abstraction | **Low** | Structural gap; functionality implemented inline without type encapsulation. |
| G18 | `ApplicableTxSetFrame` abstraction | **Low** | Structural gap; validated tx set state not encapsulated in a type. |
| G19 | Memo validation (pre-P25) | **Low** | Missing pre-P25 memo dedup check. Only affects networks below protocol 25. |
| G20 | `saveSCPHistory()` — per-slot archival persistence | **Low** | Archival concern; handled by henyey-history crate. |

### Metrics/Observability Gaps (15)

These gaps do not affect consensus or operational correctness but limit production monitoring capabilities.

| # | Gap | Severity | Impact |
|---|-----|----------|--------|
| G21 | 7 ballot phase callbacks (`ballotDidHearFromQuorum`, `nominatingValue`, `updatedCandidateValue`, `startedBallotProtocol`, `acceptedBallotPrepared`, `confirmedBallotPrepared`, `acceptedCommit`) | **Low** | No fine-grained SCP phase tracking. Cannot diagnose stuck ballots. |
| G22 | `recordSCPExecutionMetrics()` | **Low** | No SCP execution timing/counter metrics. |
| G23 | `recordSCPEvent()` / `recordSCPExternalizeEvent()` | **Low** | No per-event SCP recording. |
| G24 | `syncMetrics()` | **Low** | No periodic metric synchronization. |
| G25 | `getExternalizeLag()` / `getQsetLagInfo()` | **Low** | No externalization latency diagnostics. |
| G26 | `getPrepareStart()` | **Low** | No ballot phase timing. |
| G27 | `recordReceivedCost()` / per-validator cost tracking | **Low** | No per-validator bandwidth cost tracking. |
| G28 | `reportCostOutliersForSlot()` / K-means outlier detection | **Low** | No misbehaving validator detection. |
| G29 | `recomputeKeysToFilter()` — overlay key banning for outliers | **Low** | No automatic filtering of outlier validator traffic. |
| G30 | `getKnownQSetsCount()` / `getKnownTxSetCount()` | **Low** | No cache size accessors. |
| G31 | `getSCPQueuedEnvelopesCount()` | **Low** | No queue depth accessor. |
| G32 | Slow SCP info dump on externalization | **Low** | No diagnostic dump when consensus is slow. |
| G33 | `resolveNodeID()` — human-readable NodeID | **Low** | No NodeID-to-name resolution for logging. |
| G34 | `HerderPersistence::getNodeQuorumSet()` / `getQuorumSet()` | **Low** | No separate quorum set DB query accessors. |
| G35 | `forceRebuildQuorum()` — manual rebuild trigger | **Low** | No manual quorum rebuild. |

---

## 5. Risk Assessment

| Risk Level | Gaps | Count | Notes |
|------------|------|-------|-------|
| **High** | G1 | 1 | `getNodeWeight()` affects leader election on P22+ networks |
| **Medium** | G2, G3, G5, G6, G7, G8, G9 | 7 | Validation, optimization, and operational safety gaps |
| **Low** | G4, G10–G35 | 25 | Performance, structural, and observability gaps |

### Risk Matrix

| Scenario | Affected Gaps | Likelihood | Impact |
|----------|--------------|------------|--------|
| Wrong leader selected due to uniform weighting | G1 | Medium (P22+ only) | Medium — may slow consensus but won't break it |
| Value rejected due to uncompensated clock drift | G3 | Low (most nodes have good NTP) | Low — retried on next ballot |
| Re-processing stale envelopes under high load | G2 | Low | Low — wastes CPU, does not produce wrong results |
| Arb tx flooding degrades network | G6 | Medium | Low — affects bandwidth, not consensus |
| Applied upgrades re-proposed | G7 | High (always occurs) | Low — other nodes' validation catches this |
| Quorum intersection failure undetected | G8 | Low | High — but detection is advisory, not preventive |

---

## 6. Recommendations

### Priority 1: Consensus-Critical (address before production)

1. **G1 — `getNodeWeight()`**: Implement P22+ validator quality-based weighting for nomination. This requires reading validator quality configuration and computing weights based on home domain sizes. Without this, nomination value selection may not match stellar-core on P22+ networks.

2. **G3 — `ctValidityOffset()`**: Implement clock drift compensation for close time validation. Use the existing `drift_tracker.rs` data to adjust validation bounds. This prevents unnecessary value rejections on nodes with slight clock drift.

3. **G5 — `computeTimeout()` P23+ network config**: Read ballot timeout parameters from Soroban network config instead of using hardcoded values. Straightforward implementation given the existing config infrastructure.

### Priority 2: Operational (address for production robustness)

4. **G2 — `isNewerNominationOrBallotSt()`**: Add stale envelope detection before SCP processing. Check whether an incoming envelope's statement is newer than the current one for the same node+slot.

5. **G7 — `removeUpgrades()`**: Clear upgrade parameters after they've been applied. Add 12-hour expiry safety as stellar-core does.

6. **G8/G9 — Quorum intersection analysis**: Implement background quorum map analysis with hash-based change detection. This is an important operational safety feature for detecting quorum problems.

7. **G6 — Arb tx damping**: Implement payment loop detection via Tarjan's SCC algorithm on the asset graph, with geometric distribution dampening. This is a network-health feature.

### Priority 3: Observability (address for production monitoring)

8. **G21–G26 — SCP metrics**: Implement the ballot phase callbacks and execution metrics. These are essential for diagnosing consensus problems in production.

9. **G27–G29 — Cost tracking**: Implement per-validator cost tracking and outlier detection. This helps identify misbehaving validators.

### Intentional Deviations (Documented)

1. **Actor-model `TimerManager`**: Uses tokio channels instead of `VirtualTimer` per slot. Functionally equivalent; cleaner for async Rust architecture.

2. **Unified `TransactionQueue`**: Single struct instead of C++ inheritance hierarchy (`ClassicTransactionQueue` / `SorobanTransactionQueue`). Behavioral parity maintained; structural difference.

3. **Inline validation instead of `TxSetPhaseFrame`/`ApplicableTxSetFrame`**: Validation logic implemented directly rather than through wrapper types. Works correctly but loses type-level guarantees that a tx set has been validated.

4. **`DashMap` + `RwLock` instead of single-threaded `VirtualClock`**: Thread-safe concurrent data structures instead of stellar-core's single-threaded event loop. Architectural choice for the async Rust runtime.

---

### Test Coverage Gaps

The herder crate has significantly fewer tests than stellar-core:

| Component | Henyey | Stellar-Core | Gap |
|-----------|--------|-------------|-----|
| Herder integration | 3 tests | 34 TEST_CASE / 222 SECTION | **~95% fewer** |
| TransactionQueue | ~15 tests | Extensive | **Moderate** |
| Upgrades | 16 tests | 31 TEST_CASE / 107 SECTION | **~50% fewer** |
| SCP Driver | ~8 tests | Extensive callback tests | **Significant** |
| QuorumIntersection | 0 tests | Multiple tests | **Not started** |

Improving test coverage should accompany gap closure work, particularly for the SCP driver callbacks and herder integration scenarios.

---

## 7. v26.0.1 Delta

The following significant changes were introduced in stellar-core between v25.0.1 and v26.0.1 in `src/herder/`. This section documents their relevance and henyey's current status.

### 7.1 Parallel Tx Set Building Optimizations

**Upstream commits:** `d857bb3` (pre-sorted vector), `dfa1949` (sort-based conflict detection), `3f666252` (in-place cluster mutation), `a8241d1` (global conflict mask), `4cea3753` (O(K) tx-to-cluster lookup), `593515e` (remove unused `mBinPacking`)

stellar-core replaced `SurgePricingPriorityQueue` with a pre-sorted vector, switched from hash-map conflict detection to sort-based approach, added in-place cluster mutation with rollback, and introduced a global conflict mask for fast-path `getConflictingClusters`.

**Henyey status: ✅ Not affected.** Henyey's `parallel_tx_set_builder.rs` uses its own implementation with a custom `BitSet` for footprint conflict tracking and greedy stage assignment. These upstream changes are performance optimizations to the C++ implementation and do not change the algorithm's semantics or output. Henyey's parallel tx set building already produces correct, deterministic results matching stellar-core's output.

### 7.2 Banned Accounts (Config-Based)

**Upstream commits:** `36aeb36` (config to ban accounts), `12b3db6` (update logic), `a242cc1` (persistent ban list), `95e328b` (startup path), `a6f3426` (review comments), `4b61a66` (clear hardcoded filtered accounts)

stellar-core added the ability to ban specific accounts via configuration, with persistent storage across restarts. This prevents transactions from banned accounts from being included in transaction sets or broadcast.

**Henyey status: ❌ Not implemented.** Henyey has transaction-level banning (auto-ban after pending too long) but no account-level ban configuration. This is an operational/anti-abuse feature that does not affect consensus correctness — banned account transactions would still be rejected by other validators' validation logic. **Priority: Low** (operational convenience).

### 7.3 `computePerOpFee` Hardening

**Upstream commit:** `2ea2f61` (prevent division by zero)

stellar-core hardened `computePerOpFee` to prevent division by zero when a transaction has zero operations (which should not occur in practice but could via malformed data).

**Henyey status: ⚠️ Partial.** Henyey references `computePerOpFee` logic in `tx_queue/mod.rs` for fee calculations. The division-by-zero edge case should be verified — if henyey uses operation count as a divisor without a zero guard, this could panic on malformed transactions. **Priority: Medium** (defensive hardening).

### 7.4 TxSet Validation Improvements

**Upstream commit:** `e4e3b93` (improve TxSet validation)

stellar-core tightened transaction set validation with additional checks and clearer error reporting.

**Henyey status: ✅ Adequate.** Henyey's `tx_set.rs` has comprehensive tx set validation including phase structure, fee map validation, parallel component validation, and canonical ordering. The upstream improvements are primarily code quality and error message improvements rather than new validation rules.

### 7.5 Force Flag for Transaction Endpoint

**Upstream commit:** `ab5943c` (introduce force flag to tx endpoint)

stellar-core added a `force` flag to the transaction submission endpoint that bypasses certain queue-level filtering checks.

**Henyey status: ❌ Not implemented.** Henyey does not expose an HTTP transaction submission endpoint with a force flag. This is an operational feature for node operators who need to submit transactions that would normally be filtered. **Priority: Low** (operational convenience, not consensus-relevant).

### 7.6 Clean Up Far-Future SCP Data

**Upstream commit:** `e08742b` (clean up far-future SCP data when tracking)

stellar-core added cleanup of SCP data for slots that are far in the future relative to the current tracking slot, preventing memory accumulation from speculative/malicious envelopes.

**Henyey status: ⚠️ Partial.** Henyey has far-future slot handling in `herder.rs` (buffering far-future EXTERNALIZE messages, slot range validation) but may not perform the same aggressive cleanup of accumulated SCP state for far-future slots. **Priority: Medium** (memory safety under adversarial conditions).

### 7.7 Remove P26 Ifdefs (Protocol 26 Unconditional)

**Upstream commit:** `79258890` (remove p26 ifdefs)

stellar-core removed compile-time protocol 26 feature flags, making protocol 26 behavior unconditional.

**Henyey status: ✅ Not affected.** Henyey targets protocol 24+ and does not use compile-time feature flags for protocol behavior. Protocol-version-dependent behavior is handled at runtime via protocol version checks.

### 7.8 CAP-77 (Freeze Ledger Keys)

**Upstream commit:** `87e2161` (implement CAP-77)

stellar-core implemented the ability to freeze ledger keys via network configuration, affecting how certain accounts/keys are treated by the transaction processing pipeline.

**Henyey status: ❌ Not evaluated in this document.** CAP-77 primarily affects the transaction processing layer (`henyey-tx`) rather than the herder. The herder's role is limited to including/excluding transactions from sets, which may need to account for frozen keys during tx set construction. See the tx crate's parity documentation for CAP-77 status.

### 7.9 Other Notable Changes

| Change | Commit | Henyey Impact |
|--------|--------|---------------|
| Block validation returns explicit error codes | `61ec2e3` | ⚠️ Henyey returns `bool`/`Result` but may lack granular error codes |
| INFO log messages tracking validator timeouts | `124e0ff` | ❌ Not implemented (observability) |
| Enable parallel apply | `d458d90` | ✅ Handled by ledger/apply crate, not herder |
| Background tx signature verification | `f02f8f8` | ✅ Not herder-specific; overlay concern |
| Fee tracking logic updates | `eb70645` | ⚠️ Should verify fee tracking parity |
| Close time bug fix for slow nodes as genesis | `abb03f2` | ⚠️ Should verify henyey's genesis close time handling |
| Overlay signature verification update | `9e5838e` | ✅ Overlay concern, not herder |

### Summary of v26.0.1 Impact on Ratings

The v26.0.1 changes do not significantly alter the overall adherence ratings from the v25.0.1 evaluation. The major changes are:

1. **Parallel tx set building optimizations**: Performance-only; henyey's algorithm produces correct output.
2. **Banned accounts**: New operational feature not yet in henyey; low consensus impact.
3. **`computePerOpFee` hardening**: Minor defensive fix; should be verified in henyey.
4. **Far-future SCP cleanup**: Memory safety improvement; henyey has partial coverage.

No rating category changes are warranted. The new gaps (banned accounts, force flag, far-future cleanup) are operational in nature and do not affect the consensus-critical ratings.

---

*This evaluation was conducted against the henyey herder crate at its current state (79% function-level parity; see `crates/herder/PARITY_STATUS.md`) by comparing against stellar-core v26.0.1 reference source code.*
