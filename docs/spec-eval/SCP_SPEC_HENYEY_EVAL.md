# Henyey SCP Crate — Specification Adherence Evaluation

**Evaluated against:** `stellar-specs/SCP_SPEC.md` (Stellar Consensus Protocol Specification)
**Reference implementation:** stellar-core v26.0.1
**stellar-core pin:** v27.0.0 — evaluation body reflects the v26.0.1 baseline; Protocol 27 delta re-eval pending.
**Crate:** `crates/scp/` (henyey-scp)
**Function-level parity:** 95% (see `crates/scp/PARITY_STATUS.md` for current counts)
**Date:** 2026-05-10

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Evaluation Methodology](#2-evaluation-methodology)
3. [Section-by-Section Evaluation](#3-section-by-section-evaluation)
   - [§1 Data Types and Encoding](#31-data-types-and-encoding)
   - [§2 Quorum Structure](#32-quorum-structure)
   - [§3 Federated Agreement Primitives](#33-federated-agreement-primitives)
   - [§4 Driver Interface](#34-driver-interface)
   - [§5 Slot Model](#35-slot-model)
   - [§6 Nomination Protocol](#36-nomination-protocol)
   - [§7 Ballot Protocol](#37-ballot-protocol)
   - [§8 Message Processing](#38-message-processing)
   - [§9 Statement Ordering and Superseding](#39-statement-ordering-and-superseding)
   - [§10 Timer Model](#310-timer-model)
   - [§11 Invariants and Safety Properties](#311-invariants-and-safety-properties)
   - [§12 Constants](#312-constants)
4. [v26.0.1 Implementation Delta](#4-v2601-implementation-delta)
5. [Gap Summary](#5-gap-summary)
6. [Risk Assessment](#6-risk-assessment)
7. [Recommendations](#7-recommendations)

---

## 1. Executive Summary

The henyey SCP crate is a faithful Rust port of stellar-core's SCP library, achieving **95% parity** (see `crates/scp/PARITY_STATUS.md`) against the C++ reference implementation. The crate covers all spec-mandated behavior: the complete nomination protocol, the three-phase ballot protocol state machine (`PREPARE` -> `CONFIRM` -> `EXTERNALIZE`), federated agreement primitives, quorum set operations, statement ordering, timer management, and state recovery.

The evaluation finds that the Rust implementation adheres to the specification with very high fidelity. All MUST-level requirements are satisfied. The few deviations are intentional architectural adaptations (Rust ownership model replacing C++ wrapper classes, `SlotContext` parameter passing replacing back-references, serde replacing jsoncpp) that preserve behavioral equivalence while being idiomatic Rust.

### Overall Adherence Rating

| Category | Rating | Notes |
|----------|--------|-------|
| **Data Types & Encoding** | **Full** | XDR types from `stellar_xdr::curr`; ballot comparison, compatibility all correct |
| **Quorum Structure** | **Full** | `is_quorum_slice`, `is_v_blocking`, `is_quorum` with fixed-point iteration, `find_closest_v_blocking`, normalization, sanity checks all match spec |
| **Federated Agreement** | **Full** | `federated_accept` and `federated_ratify` implemented per spec on both Slot and BallotProtocol |
| **Driver Interface** | **Full** | All pure virtual and virtual-with-defaults methods mapped to `SCPDriver` trait; `ValidationLevel` enum matches spec |
| **Slot Model** | **Full** | Per-slot state with nomination + ballot protocols, `fully_validated` flag, `got_v_blocking` flag, envelope routing, crash recovery, purging |
| **Nomination Protocol** | **Full** | Round leaders, priority hash, cumulative leader set, value adoption from leaders, federated accept/ratify for value promotion, composite candidate, stop nomination |
| **Ballot Protocol** | **Full** | Complete `advanceSlot` state machine with all 5 steps in strict order; recursion guard at 50; `attemptBump` only at level 1; all ballot state variables match whitepaper (b, p, p', h, c) |
| **Message Processing** | **Full** | Envelope reception with signature verification, sanity checks, value validation, freshness, EXTERNALIZE compatibility gate |
| **Statement Ordering** | **Full** | `is_newer_nomination_or_ballot_st` with correct type ranking and per-type lexicographic comparison |
| **Timer Model** | **Full** | Nomination and ballot timers with correct setup/cancel semantics; `checkHeardFromQuorum` logic |
| **Invariants** | **Full** | `check_invariants()` present; value locking via `value_override`; commit voiding; phase transition irreversibility |
| **Constants** | **Full** | `MAX_ADVANCE_SLOT_RECURSION = 50`, `MAXIMUM_QUORUM_NESTING_LEVEL = 4`, `MAXIMUM_QUORUM_NODES = 1000`, timer IDs match |

**Estimated spec adherence: ~99%.** The remaining ~1% consists of intentional architectural differences that do not affect observable protocol behavior.

---

## 2. Evaluation Methodology

This evaluation compares the henyey SCP implementation against `stellar-specs/SCP_SPEC.md` (Sections 1-16, 1889 lines), cross-referenced with the C++ implementation (stellar-core v26.0.1) via the crate's `PARITY_STATUS.md`.

Every section of the specification was checked against the corresponding Rust module. Each requirement was assessed on:

1. **Structural completeness**: Are the required data structures, state variables, and abstractions present?
2. **Behavioral correctness**: Do the algorithms, state transitions, and edge cases match the spec?
3. **Constant fidelity**: Do hardcoded values, thresholds, and limits match?

Ratings per requirement:

| Symbol | Meaning |
|--------|---------|
| ✅ | Fully implemented and matches specification |
| ⚠️ | Partially implemented or minor deviation |
| ❌ | Not implemented |
| ➖ | Not applicable (intentional architectural departure) |

Source file references use the format `file.rs:line`.

---

## 3. Section-by-Section Evaluation

### 3.1 Data Types and Encoding

**Spec Section:** §3 (Data Types and Encoding)
**Source files:** `lib.rs` (re-exports), XDR types from `stellar_xdr::curr`

The spec defines SCP's wire types: `SCPBallot`, `SCPNomination`, `SCPStatement`, `SCPEnvelope`, and `SCPQuorumSet`. All are consumed via the `stellar_xdr` crate.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `SCPBallot` with `counter` and `value` fields | ✅ | `ScpBallot` from `stellar_xdr::curr` re-exported at `lib.rs:203` |
| Ballot total ordering: lexicographic on `(counter, value)` | ✅ | `ballot_compare()` in `ballot/statements.rs` compares `counter` then `value` |
| Ballot compatibility: `b1 ~ b2` iff `b1.value == b2.value` | ✅ | `ballot_compatible()` in `ballot/statements.rs` |
| Less-and-compatible (`≲`) and less-and-incompatible (`≨`) | ✅ | `are_ballots_less_and_compatible()` and `are_ballots_less_and_incompatible()` in `ballot/statements.rs` |
| `SCPNomination` with sorted `votes` and `accepted` | ✅ | `ScpNomination` from XDR; sort enforcement in `is_sane_statement()` |
| `SCPStatement` union with 4 types | ✅ | `ScpStatementPledges` enum: `Nominate`, `Prepare`, `Confirm`, `Externalize` |
| `SCPEnvelope` wrapping statement + signature | ✅ | `ScpEnvelope` from XDR |

### 3.2 Quorum Structure

**Spec Section:** §4 (Quorum Structure)
**Source files:** `quorum.rs`, `quorum_config.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `SCPQuorumSet` with threshold, validators, inner sets | ✅ | XDR type; normalization in `quorum.rs` |
| Quorum slice detection: node's own set must be satisfied | ✅ | `is_quorum_slice()` |
| V-blocking: at least one set has a member in the candidate set | ✅ | `is_v_blocking()` |
| Quorum test: fixed-point iteration over all reachable nodes | ✅ | `is_quorum()` with iterative node-deletion |
| `findClosestVBlocking`: minimal v-blocking subset | ✅ | `find_closest_v_blocking()` |
| Quorum set normalization: flatten and deduplicate | ✅ | `normalize_quorum_set()`, `normalize_quorum_set_with_remove()` |
| Sanity check: threshold > 0, reasonable nesting/size | ✅ | `is_quorum_set_sane()` with `MAXIMUM_QUORUM_NESTING_LEVEL` and `MAXIMUM_QUORUM_NODES` |

### 3.3 Federated Agreement Primitives

**Spec Section:** §5 (Federated Agreement Primitives)
**Source files:** `slot.rs`, `ballot/state_machine.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `federatedAccept(voted, accepted)`: quorum voted OR v-blocking accepted | ✅ | `federated_accept()` in slot.rs and ballot protocol |
| `federatedRatify(voted)`: quorum unanimously voted | ✅ | `federated_ratify()` in slot.rs and ballot protocol |
| Correct application to nomination (vote → accept → ratify) | ✅ | Nomination protocol uses both primitives for value promotion |
| Correct application to ballot protocol (prepare, commit) | ✅ | Ballot protocol uses both primitives for state transitions |

### 3.4 Driver Interface

**Spec Section:** §6 (Driver Interface)
**Source files:** `driver.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Pure virtual: `validateValue`, `combineValues`, `extractValidValue` | ✅ | Required trait methods on `SCPDriver` |
| Pure virtual: `computeHashNode`, `computeValueHash` | ✅ | `compute_hash_node()`, `compute_value_hash()` |
| Pure virtual: `emitEnvelope`, `setupTimer`, `stopTimer` | ✅ | Trait methods with corresponding semantics |
| Virtual with default: `getNodeWeight` | ✅ | `get_node_weight()` with default implementation |
| `ValidationLevel` enum: `kInvalidValue`, `kFullyValidatedValue`, `kMaybeValidValue` | ✅ | `ValidationLevel` enum with matching variants |
| `toShortString`, `getValueString` | ✅ | Trait methods for node/value display |
| `strip_all_upgrades` for upgrade stripping | ✅ | `strip_all_upgrades()` driver method with `Option<Value>` return |

### 3.5 Slot Model

**Spec Section:** §7 (Slot Model)
**Source files:** `slot.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Per-slot state encapsulating nomination + ballot | ✅ | `Slot` struct with `NominationProtocol` + `BallotProtocol` |
| `fullyValidated` flag | ✅ | `fully_validated: bool` on Slot |
| `gotVBlocking` flag for first v-blocking statement | ✅ | `got_v_blocking: bool` on Slot |
| Envelope routing: nominate → NominationProtocol, ballot → BallotProtocol | ✅ | `receive_envelope()` dispatches by pledge type |
| State recovery from persisted envelopes | ✅ | `set_state_from_envelope()` for crash recovery |
| Purge: clean up old slot state | ✅ | `purge_slots()` on SCP struct |

### 3.6 Nomination Protocol

**Spec Section:** §8 (Nomination Protocol)
**Source files:** `nomination.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Round leaders: priority-based selection from quorum set | ✅ | `update_round_leaders()` with priority hash |
| Cumulative leader set: rounds only add leaders | ✅ | `round_leaders` is extended, never reset within a slot |
| Value adoption: accept values from leaders | ✅ | `get_new_value_from_nomination()` checks leader status |
| `federatedAccept` for vote → accept promotion | ✅ | Applied in `process_nomination_statement()` |
| `federatedRatify` for accept → candidate promotion | ✅ | Applied in `process_nomination_statement()` |
| Composite candidate: combine all ratified values | ✅ | `combine_candidates()` called via driver |
| Stop nomination when candidate produced | ✅ | `stopped` flag set when candidates non-empty |
| Timeout doubling per round | ✅ | Timer duration doubles each round |
| Upgrade stripping on high timeouts | ✅ | `strip_all_upgrades()` called when over timeout limit |

### 3.7 Ballot Protocol

**Spec Section:** §9 (Ballot Protocol)
**Source files:** `ballot/state_machine.rs`, `ballot/mod.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Three phases: PREPARE → CONFIRM → EXTERNALIZE | ✅ | `BallotPhase` enum; irreversible transitions |
| State variables: b, p, p', h, c | ✅ | `current_ballot`, `prepared`, `prepared_prime`, `high_ballot`, `commit` |
| `advanceSlot` 5-step sequence | ✅ | Steps 1-5 in strict order in `advance_slot()` |
| Step 1: accept prepare(b) via federated accept | ✅ | `attempt_accept_prepared()` |
| Step 2: confirm prepare(b) via federated ratify | ✅ | `attempt_confirm_prepared()` |
| Step 3: accept commit(b) via federated accept | ✅ | `attempt_accept_commit()` |
| Step 4: confirm commit(b) via federated ratify | ✅ | `attempt_confirm_commit()` |
| Step 5: externalize | ✅ | `attempt_externalize()` |
| Recursion guard: MAX_ADVANCE_SLOT_RECURSION = 50 | ✅ | `MAX_PROTOCOL_TRANSITIONS = 50`; panics on exceed |
| `attemptBump`: respond to v-blocking with higher ballot | ✅ | `attempt_bump()` at level 1 only |
| `bumpState`: create/update ballot from new value | ✅ | `bump_state()` with `value_override` |
| `heardFromQuorum`: ballot timer management | ✅ | `check_heard_from_quorum()` |

### 3.8 Message Processing

**Spec Section:** §10 (Message Processing)
**Source files:** `slot.rs`, `scp.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Envelope reception with signature verification | ✅ | `verify_envelope()` via driver |
| Sanity checks on incoming statements | ✅ | `is_sane_statement()` |
| Value validation before processing | ✅ | `validate_value()` call with `ValidationLevel` |
| Freshness check: reject old/duplicate envelopes | ✅ | `is_newer_statement_for_node()` check |
| EXTERNALIZE compatibility gate | ✅ | Only compatible envelopes accepted in EXTERNALIZE phase |
| Broadcast after local state change | ✅ | `emit_envelope()` called on state transitions |

### 3.9 Statement Ordering and Superseding

**Spec Section:** §11 (Statement Ordering and Superseding)
**Source files:** `slot.rs`, `nomination.rs`, `ballot/statements.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Statement type ranking: Nominate < Prepare < Confirm < Externalize | ✅ | `is_newer_nomination_or_ballot_st()` |
| Per-type ordering: lexicographic on relevant fields | ✅ | Type-specific comparison in `is_newer_statement()` |
| Nomination ordering: superset of votes + accepted is newer | ✅ | `is_newer_nominate()` |
| Ballot ordering: phase + ballot counter + state variables | ✅ | Per-pledge-type comparison logic |

### 3.10 Timer Model

**Spec Section:** §12 (Timer Model)
**Source files:** `driver.rs`, `nomination.rs`, `ballot/mod.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Nomination timer: fires to advance round | ✅ | `SCPTimerType::Nomination` with round-doubling duration |
| Ballot timer: fires to bump ballot counter | ✅ | `SCPTimerType::Ballot` |
| Timer setup: delegate to driver | ✅ | `setup_timer()` trait method |
| Timer cancel: delegate to driver | ✅ | `stop_timer()` trait method |
| `checkHeardFromQuorum`: cancel ballot timer when quorum heard | ✅ | `check_heard_from_quorum()` logic |

### 3.11 Invariants and Safety Properties

**Spec Section:** §13 (Invariants and Safety Properties)
**Source files:** `ballot/state_machine.rs`, `nomination.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §13.1 Inv 1: b.counter >= h.counter >= c.counter | ✅ | Enforced in `set_confirm_prepared()`, `set_accept_commit()` |
| §13.1 Inv 2: h ~ c (compatible ballots) | ✅ | Commit only set when compatible with h |
| §13.1 Inv 3: c.counter > 0 ⟹ b ~ c | ✅ | Current ballot always compatible with commit when set |
| §13.1 Inv 4: b ~ p if p set | ✅ | `value_override` ensures compatibility |
| §13.1 Inv 5: p.counter >= p'.counter | ✅ | p' only updated when dominated by p |
| §13.1 Inv 6: All of b, p, c, h exist in CONFIRM/EXTERNALIZE | ✅ | Enforced by state machine transitions |
| §13.2 Inv 1: Monotonic growth of nomination statements | ✅ | `is_newer_nominate()` enforces superset property |
| §13.2 Inv 3: Candidates freeze nomination | ✅ | `if candidates.len() > 0: return false` in `nominate()` |
| §13.2 Inv 4: Timer captures original value | ✅ | Timer captures `value` and `previous_value` from initial call |
| §13.3 Phase transitions: PREPARE → CONFIRM → EXTERNALIZE (irreversible) | ✅ | Phase only advances, never retreats |
| §13.4 Value locking: `mValueOverride` enforced in `bumpState` | ✅ | `value_override` replaces value in `bump_state()` |
| §13.5 Commit voiding: reset c when p/p' incompatible with h | ✅ | `set_accept_prepared()` at `state_machine.rs:120-137` clears `commit` |
| §13.6 EXTERNALIZE finality: no state transitions, only compatible envelopes | ✅ | EXTERNALIZE gate rejects incompatible envelopes |

### 3.12 Constants

**Spec Section:** §14 (Constants)
**Source files:** `ballot/mod.rs`, `quorum.rs`, `driver.rs`

| Constant | Spec Value | Impl Value | Status |
|----------|-----------|------------|--------|
| `MAX_ADVANCE_SLOT_RECURSION` | 50 | `MAX_PROTOCOL_TRANSITIONS = 50` | ✅ |
| `MAXIMUM_QUORUM_NESTING_LEVEL` | 4 | `MAXIMUM_QUORUM_NESTING_LEVEL = 4` | ✅ |
| `MAXIMUM_QUORUM_NODES` (max validators) | 1000 | `MAXIMUM_QUORUM_NODES = 1000` | ✅ |
| `NOMINATION_TIMER` | 0 | `SCPTimerType::Nomination` | ✅ |
| `BALLOT_PROTOCOL_TIMER` | 1 | `SCPTimerType::Ballot` | ✅ |
| `hash_N` (neighborhood) | 1 | Delegated to driver `compute_hash_node(is_priority=false)` | ✅ |
| `hash_P` (priority) | 2 | Delegated to driver `compute_hash_node(is_priority=true)` | ✅ |
| `hash_K` (value) | 3 | Delegated to driver `compute_value_hash()` | ✅ |
| `NUM_TIMEOUTS_THRESHOLD_FOR_REPORTING` | 2 | Implementation-specific (info reporting) | ✅ |
| `NUM_SLOTS_TO_CHECK_FOR_REPORTING` | 2 | Implementation-specific (info reporting) | ✅ |

---

## 4. v26.0.1 Implementation Delta

stellar-core v26.0.1 introduced the following SCP-layer changes relative to v25.0.1 (9 commits touching `src/scp/`). The SCP protocol semantics were **not** changed — these are all bug fixes, operational improvements, or cleanup:

| Commit | Description | Protocol Impact | Henyey Status |
|--------|-------------|-----------------|---------------|
| `437988f` | Exclude zero-weight nodes from max round leader calculation | Bug fix (liveness) | ⚠️ **Not yet ported** — henyey uses old `1 + count_all_nodes` at `nomination.rs:957`. Also adds a 1000-iteration safety cap not present in henyey. |
| `83df510` | Add options for stripping upgrades from Values | Operational (liveness under high timeouts) | ✅ Implemented — `strip_all_upgrades()` driver method + nomination integration |
| `4ebed1c` | Simplify `overUpgradeTimeoutLimit` logic | Cleanup | ✅ Equivalent logic present |
| `e08742b` | Clean up far-future SCP data when tracking | Operational (memory) | ➖ Handled via `purge_slots()` in the herder layer |
| `b77d6cb` | Add INFO log messages tracking validator timeouts | Observability | ➖ Equivalent tracing in henyey's herder |
| `648fba1` | Miscellaneous cleanup | Cleanup | ➖ No behavioral change |
| `b86381c` | Fix SCP logging | Logging fix | ➖ Different logging framework |
| `f0049e8` | Fix SCP logging | Logging fix | ➖ Different logging framework |
| `a52d8d7` | Small clean ups | Cleanup | ➖ No behavioral change |

### Action Items

1. **Port zero-weight leader fix** (`437988f`): The current `max_leader_count` calculation at `nomination.rs:957` counts all nodes unconditionally. When quorum sets contain zero-weight (LOW quality) validators, this can cause infinite fast-timeouts as the leader election loop will never pick those nodes. The fix should:
   - Count only nodes with `get_node_weight() > 0` in `max_leader_count`
   - Add a 1000-iteration safety cap on the while loop as a defensive measure

   This is a **liveness** issue, not a safety issue — consensus correctness is not affected, but nomination performance degrades when zero-weight nodes are present.

---

## 5. Gap Summary

### Critical Gaps

**None identified.** All MUST-level requirements from the specification are implemented.

### Moderate Gaps

| # | Gap | Impact | Status |
|---|-----|--------|--------|
| 1 | Zero-weight node exclusion from max leader count (v26.0.1 fix) | Liveness degradation with LOW-quality validators | Needs port |

### Minor Gaps / Architectural Differences

These are intentional deviations that do not affect protocol correctness or interoperability:

| # | Gap | Spec Reference | Impact | Justification |
|---|-----|----------------|--------|---------------|
| 1 | **LocalNode dissolved** — Node identity fields stored on `SCP` struct; quorum operations are free functions in `quorum.rs` | §6, §7 | None | Avoids C++ `shared_ptr<LocalNode>` indirection; more idiomatic Rust |
| 2 | **Back-references eliminated** — Protocols receive context via `SlotContext` parameter instead of `Slot&` / `SCP&` references | §7, §9 | None | Rust borrow checker requires this pattern; equivalent functionality |
| 3 | **Wrapper types replaced** — No `ValueWrapper`, `SCPEnvelopeWrapper`, `SCPBallotWrapper`; owned types with `Clone` used instead | §3 | None | Rust ownership model; no shared pointer needed |
| 4 | **JSON via serde** — Structured `SlotInfo`, `BallotInfo`, `NominationInfo` types instead of manual jsoncpp construction | §7 (getJsonInfo) | None | Type-safe serialization; same semantic content |
| 5 | **Error handling** — `Result` types and `tracing` instead of `releaseAssert()` for most cases; panic only for `MAX_ADVANCE_SLOT_RECURSION` | §9.4 | Minimal | Graceful degradation vs process abort. The recursion guard still panics, matching stellar-core's fatal behavior for this case |
| 6 | **Timer delegation** — Timer setup/stop are trait methods with no-op defaults; the herder provides timer management externally | §12 | None | SCP is a library; timer management is properly delegated to the driver |
| 7 | **`stopped` field in NominationProtocol** — Extra `stopped: bool` field not in spec, separate from `started` | §8.8 | None | Rust implementation detail for clearer state tracking; behavioral equivalence maintained |

---

## 6. Risk Assessment

### Consensus Safety Risk: **Negligible**

The SCP crate implements all consensus-critical algorithms faithfully:

- **Federated agreement primitives** (`federatedAccept`, `federatedRatify`) are correct.
- **Ballot protocol state machine** follows the exact 5-step `advanceSlot` sequence with proper recursion limits.
- **Value locking** via `value_override` prevents conflicting commits.
- **Commit voiding** correctly resets the commit range when superseded by incompatible prepared ballots.
- **Phase transitions** are irreversible and match the spec exactly.
- **Quorum operations** (transitive quorum test, v-blocking) use the correct fixed-point iteration.

### Liveness Risk: **Low**

- **Nomination round leaders** are computed with the correct cumulative, fast-timeout algorithm.
- **Known issue**: Zero-weight nodes in quorum sets can cause excessive fast-timeouts (see §4 above). This is a pre-existing stellar-core bug fixed in v26.0.1 that has not yet been ported.
- **Ballot bumping** (`attemptBump`) correctly responds to v-blocking sets ahead of the local node.
- **Timer management** is properly delegated to the driver, with correct setup and cancellation semantics.
- **Upgrade stripping** prevents liveness degradation during extended timeout periods.

### Interoperability Risk: **None**

- All XDR types are from the canonical `stellar_xdr` crate.
- Statement sanity checks match spec exactly.
- Statement ordering is implemented per spec, ensuring correct deduplication.

### Test Coverage: **Comprehensive**

Per `PARITY_STATUS.md`: 353 total tests (173 unit + 180 integration), covering all protocol areas including multi-node simulations and parity-focused scenarios. The test suite exceeds the upstream test count.

---

## 7. Recommendations

### Action Required

1. **Port zero-weight leader fix**: Implement the v26.0.1 fix from commit `437988f` to exclude zero-weight nodes from `max_leader_count` in `update_round_leaders()` and add the 1000-iteration safety cap. This is a liveness fix relevant to networks with LOW-quality validators in quorum sets.

### No Action Required

All other spec requirements are fully satisfied. No consensus-critical, interoperability, or additional liveness gaps exist beyond the item above.

### Future Considerations

1. **Property-based testing**: Consider adding proptest/quickcheck tests for quorum set operations and ballot state machine transitions to catch edge cases beyond the current deterministic test suite.

2. **Formal invariant checking**: The `check_invariants()` method in the ballot protocol could be called more aggressively in debug builds (e.g., after every state transition) to catch regression bugs early.

3. **Spec evolution tracking**: As stellar-core advances beyond v26.0.1, the SCP library should be re-evaluated for any new protocol requirements (e.g., changes to timeout computation, quorum set constraints, or new statement types).
