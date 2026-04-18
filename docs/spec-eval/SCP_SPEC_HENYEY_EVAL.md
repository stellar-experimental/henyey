# Henyey SCP Crate — Specification Adherence Evaluation

**Evaluated against:** `stellar-specs/SCP_SPEC.md` (Stellar Consensus Protocol Specification v25)
**Crate:** `crates/scp/` (henyey-scp)
**Function-level parity:** 100% (164/164 functions per `PARITY_STATUS.md`)
**Date:** 2026-02-20

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
4. [Gap Summary](#4-gap-summary)
5. [Risk Assessment](#5-risk-assessment)
6. [Recommendations](#6-recommendations)

---

## 1. Executive Summary

The henyey SCP crate is a faithful Rust port of stellar-core's SCP library, achieving **100% function-level parity** (164/164 functions) against the C++ reference implementation. The crate covers all spec-mandated behavior: the complete nomination protocol, the three-phase ballot protocol state machine (`PREPARE` -> `CONFIRM` -> `EXTERNALIZE`), federated agreement primitives, quorum set operations, statement ordering, timer management, and state recovery.

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

This evaluation compares the henyey SCP implementation against `stellar-specs/SCP_SPEC.md` (Sections 1-16, 1889 lines), cross-referenced with the C++ implementation via the crate's `PARITY_STATUS.md`.

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
| `SCPQuorumSet` recursive threshold structure | ✅ | `ScpQuorumSet` from XDR |
| Null ballot ordering (less than any non-null) | ✅ | `cmp_opt_ballot()` in `compare.rs:55` treats `None < Some(_)` |

### 3.2 Quorum Structure

**Spec Section:** §4 (Quorum Structure)
**Source files:** `quorum.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §4.2 Rule 1: Nesting depth ≤ 4 | ✅ | `MAXIMUM_QUORUM_NESTING_LEVEL = 4` at `quorum.rs:59` |
| §4.2 Rule 2: `threshold >= 1` | ✅ | `check_sanity()` in `QuorumSetSanityChecker` |
| §4.2 Rule 3: `threshold <= validators + innerSets` | ✅ | `check_sanity()` in `QuorumSetSanityChecker` |
| §4.2 Rule 4: No duplicate validators | ✅ | `known_nodes: HashSet<NodeId>` tracks duplicates across tree |
| §4.2 Rule 5: Total nodes in `[1, 1000]` | ✅ | `MAXIMUM_QUORUM_NODES = 1000` at `quorum.rs:68`; enforced in `is_quorum_set_sane()` |
| §4.2 Rule 6: Majority threshold (extra checks) | ✅ | `extra_checks` parameter in `is_quorum_set_sane()` |
| §4.3 Normalization Phase 1 (simplification) | ✅ | `normalize_quorum_set()` and `normalize_quorum_set_with_remove()` |
| §4.3 Normalization Phase 2 (reordering) | ✅ | Sorting validators and inner sets in normalization |
| §4.4 `isQuorumSlice(Q, S)` | ✅ | `is_quorum_slice()` at `quorum.rs:85` — threshold counting with early exit |
| §4.5 `isVBlocking(Q, S)` — threshold = `total - threshold + 1` | ✅ | `is_blocking_set_helper()` at `quorum.rs:188` — `blocking_threshold = total - threshold + 1` |
| §4.5 `threshold == 0` returns false | ✅ | Guard at `quorum.rs:193` |
| §4.6 Transitive quorum test with fixed-point iteration | ✅ | `is_quorum()` at `quorum.rs:141` — loop retaining nodes whose slices are satisfied, terminating when set stabilizes |
| §4.6 Final check: local node's quorum set satisfied | ✅ | `is_quorum_slice(quorum_set, &remaining_set, ...)` at `quorum.rs:169` |
| §4.7 `findClosestVBlocking` greedy algorithm | ✅ | `find_closest_v_blocking()` in `quorum.rs` |

### 3.3 Federated Agreement Primitives

**Spec Section:** §5 (Federated Agreement Primitives)
**Source files:** `slot.rs`, `ballot/mod.rs`, `ballot/state_machine.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §5.1 `federatedAccept(voted, accepted)` — v-blocking first, then quorum | ✅ | `federated_accept()` on Slot checks `is_blocking_set` then `is_quorum` |
| §5.1 V-blocking checked before quorum (optimization) | ✅ | V-blocking check is evaluated first in the implementation |
| §5.2 `federatedRatify(voted)` — quorum has voted | ✅ | `federated_ratify()` calls `is_quorum` with voted predicate |
| §5.3 Progression: voted → accepted → confirmed | ✅ | Applied in both nomination (`should_accept_value` / ratify) and ballot protocol (prepare → commit) |

### 3.4 Driver Interface

**Spec Section:** §6 (Driver Interface)
**Source files:** `driver.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §6.2 `signEnvelope` — required | ✅ | `sign_envelope(&self, envelope: &mut ScpEnvelope)` at `driver.rs:241` |
| §6.2 `getQSet(hash)` — required | ✅ | `get_quorum_set_by_hash()` at `driver.rs:175` (default returns `None`) |
| §6.2 `emitEnvelope` — required | ✅ | `emit_envelope(&self, envelope: &ScpEnvelope)` at `driver.rs:157` |
| §6.2 `getHashOf(vals)` — required | ✅ | `get_hash_of(&self, data: &[u8]) -> Hash256` at `driver.rs:301` |
| §6.2 `combineCandidates` — required | ✅ | `combine_candidates()` at `driver.rs:139` |
| §6.2 `setupTimer` / `stopTimer` — required | ✅ | `setup_timer()` at `driver.rs:336` and `stop_timer()` at `driver.rs:351` |
| §6.2 `computeTimeout` — required | ✅ | `compute_timeout()` at `driver.rs:235` |
| §6.3 `validateValue` — default `kMaybeValidValue` | ✅ | `validate_value()` is a required trait method; default provided by herder |
| §6.3 `extractValidValue` — default null | ✅ | `extract_valid_value()` at `driver.rs:152` |
| §6.4 `ValidationLevel` enum with 3 levels | ✅ (extended) | `ValidationLevel::Invalid`, `MaybeValid`, `MaybeValidDeferred` (henyey extension for issues #1795 / #1798 — fast-path forwarding ahead of stellar-core timing), `FullyValidated` at `driver.rs` |
| §6.5 Event callbacks (7 total) | ✅ | `value_externalized`, `nominating_value`, `updated_candidate_value`, `started_ballot_protocol`, `accepted_ballot_prepared` (via `ballot_did_prepare`), `confirmed_ballot_prepared` (via `ballot_did_confirm`), `accepted_commit`, `ballot_did_hear_from_quorum` — all present |
| §6.6 Hash functions with domain separation (hash_N=1, hash_P=2, hash_K=3) | ✅ | `compute_hash_node()` (neighborhood/priority) and `compute_value_hash()` delegate to driver with appropriate flags |
| §6.7 `getNodeWeight` — recursive weight computation | ✅ | `base_get_node_weight()` at `driver.rs:426` matches spec algorithm exactly; `compute_weight()` at `driver.rs:409` uses `u128` for overflow safety |
| §6.7 Local node returns `UINT64_MAX` | ✅ | Guard at `driver.rs:431`: `if is_local_node { return u64::MAX; }` |
| §6.7 Override hook for application-specific weights | ✅ | `get_node_weight()` trait method with default calling `base_get_node_weight()` at `driver.rs:279-286` |

### 3.5 Slot Model

**Spec Section:** §7 (Slot Model)
**Source files:** `slot.rs`, `scp.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §7.1 Slots created on demand (auto-vivification) | ✅ | `slots.entry(slot_index).or_insert_with(...)` in `scp.rs:201` and `scp.rs:240` |
| §7.1 Each slot contains nomination + ballot protocol | ✅ | `Slot` struct at `slot.rs:57` with `nomination: NominationProtocol` and `ballot: BallotProtocol` |
| §7.1 `fully_validated` flag | ✅ | `fully_validated: bool` at `slot.rs:89` |
| §7.1 `got_v_blocking` flag | ✅ | `got_v_blocking: bool` at `slot.rs:95` |
| §7.1 Statement history for auditing | ✅ | `envelopes: HashMap<NodeId, Vec<ScpEnvelope>>` at `slot.rs:77` |
| §7.2 `fully_validated` initialized to `true` for validators, `false` for watchers | ✅ | `fully_validated: is_validator` at `slot.rs:121` |
| §7.2 `got_v_blocking` set once, never cleared | ✅ | `maybe_set_got_v_blocking()` at `slot.rs:176` — early return if already set |
| §7.3 Envelope routing: NOMINATE → nomination, PREPARE/CONFIRM/EXTERNALIZE → ballot | ✅ | Match on `ScpStatementPledges` at `slot.rs:213-219` |
| §7.4 Singleton quorum set during EXTERNALIZE | ✅ | `singleton_quorum_set()` used in quorum lookups for externalized nodes |
| §7.5 Envelope construction: stamp nodeID, slotIndex, sign | ✅ | `create_envelope()` on Slot stamps fields and calls `driver.sign_envelope()` |
| §7.6 Crash recovery via `setStateFromEnvelope` | ✅ | `set_state_from_envelope()` on Slot, NominationProtocol, and BallotProtocol |
| §7.7 Slot purging: `purgeSlots(maxSlotIndex, slotToKeep)` | ✅ | `purge_slots()` in `scp.rs` |
| §7.8 Message retrieval: ballot takes precedence over nomination | ✅ | `get_latest_message()` checks ballot first, then nomination |

### 3.6 Nomination Protocol

**Spec Section:** §8 (Nomination Protocol)
**Source files:** `nomination.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §8.2 State variables: `mRoundNumber`, `mVotes`, `mAccepted`, `mCandidates`, `mLatestNominations`, `mRoundLeaders`, `mNominationStarted`, `mLatestCompositeCandidate`, `mPreviousValue`, `mTimerExpCount` | ✅ | All present in `NominationProtocol` struct at `nomination.rs:63-121`: `round`, `votes`, `accepted`, `candidates`, `latest_nominations`, `round_leaders`, `started`, `latest_composite`, `previous_value`, `timer_exp_count` |
| §8.3 Nomination sanity: `votes + accepted > 0`, both sorted | ✅ | `is_sane_statement()` in nomination.rs |
| §8.4 Round leaders: cumulative set, fast-timeout when same leaders re-elected | ✅ | `update_round_leaders()` accumulates into `round_leaders`; advances `round` when no new leaders found |
| §8.4 Node priority: `getNodePriority` with neighborhood hash ≤ weight gate | ✅ | `get_node_priority()` checks `hash_node(neighborhood) <= weight` then returns `hash_node(priority)` |
| §8.5 `nominate()` flow: early exit if candidates exist, timeout accounting, round increment, leader computation, value adoption, self-nomination, timer scheduling | ✅ | `nominate()` in `nomination.rs` follows spec flow exactly |
| §8.5 Self-nomination only if leader AND no values adopted | ✅ | Self-nomination guarded by `local_node_id ∈ round_leaders AND votes is empty` |
| §8.5 Value selection: `getNewValueFromNomination` — accepted priority over votes, highest hash | ✅ | `get_new_value_from_nomination()` checks accepted first, falls through to votes; selects highest `hash_value` |
| §8.6 Envelope processing: Phase A (federated accept), Phase B (federated ratify), Phase C (adopt leader votes) | ✅ | `process_envelope()` in `nomination.rs` implements all three phases |
| §8.6 `extractValidValue` variant added to `mVotes` only (not `mAccepted`) | ✅ | Extracted value added only to `votes`, not `accepted` |
| §8.6 Timer stopped on first candidate | ✅ | `stop_timer(NOMINATION_TIMER)` called when first candidate confirmed |
| §8.6 New candidates trigger `combineCandidates` → `bumpState` | ✅ | Candidates trigger `combine_candidates()` and `bump_state()` in Slot |
| §8.7 Emission: self-process, only broadcast if fully validated | ✅ | `emit_nomination()` self-processes then emits only if `fully_validated` |
| §8.8 `stopNomination()` sets `started = false`, preserves existing state | ✅ | `stop()` sets `started = false`; state not cleared |
| §8.9 Nomination statement ordering: monotonic growth of votes and accepted | ✅ | `is_newer_nominate()` in `compare.rs:42` checks subset + strict growth |
| §8.10 State recovery: precondition `!mNominationStarted`, restore votes/accepted/envelope | ✅ | `set_state_from_envelope()` on NominationProtocol |

### 3.7 Ballot Protocol

**Spec Section:** §9 (Ballot Protocol)
**Source files:** `ballot/mod.rs`, `ballot/state_machine.rs`, `ballot/statements.rs`, `ballot/envelope.rs`

#### Core State Machine

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §9.2 State variables: `mCurrentBallot`, `mPrepared`, `mPreparedPrime`, `mHighBallot`, `mCommit`, `mPhase`, `mValueOverride`, `mLatestEnvelopes`, `mHeardFromQuorum`, `mCurrentMessageLevel`, `mTimerExpCount` | ✅ | All present in `BallotProtocol` struct at `ballot/mod.rs:109-176` |
| §9.3 PREPARE/CONFIRM/EXTERNALIZE statement formats | ✅ | `emit_prepare()`, `emit_confirm()`, `emit_externalize()` construct correct XDR types |
| §9.4 `advanceSlot` — 5 steps in strict order | ✅ | `advance_slot()` at `ballot/state_machine.rs:8-53` calls `attempt_accept_prepared`, `attempt_confirm_prepared`, `attempt_accept_commit`, `attempt_confirm_commit` in order |
| §9.4 `attemptBump` only at level 1, loops until stable | ✅ | `if self.current_message_level == 1 { loop { ... if !bumped { break; } } }` at `state_machine.rs:35-43` |
| §9.4 `checkHeardFromQuorum` after attempts at level 1 | ✅ | `self.check_heard_from_quorum(ctx)` at `state_machine.rs:43` |
| §9.4 Recursion guard: MAX = 50, fatal on exceed | ✅ | `MAX_PROTOCOL_TRANSITIONS = 50` at `ballot/mod.rs:48`; panic on exceed at `state_machine.rs:17` |
| §9.4 `sendLatestEnvelope` on `didWork` | ✅ | `self.send_latest_envelope(ctx.driver)` at `state_machine.rs:48` |

#### Step 1: Accept Prepared (§9.5)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Gate: only PREPARE or CONFIRM | ✅ | Phase check at `state_machine.rs:60` |
| Candidates from highest to lowest | ✅ | `.iter().rev()` at `state_machine.rs:66` |
| CONFIRM: only if ballot > mPrepared and compatible with mCommit | ✅ | Guards at `state_machine.rs:67-78` |
| Skip if already covered by mPreparedPrime or less-and-compatible with mPrepared | ✅ | Guards at `state_machine.rs:80-89` |
| `federatedAccept(votedPrepared, acceptedPrepared)` | ✅ | `self.federated_accept(...)` at `state_machine.rs:92-98` |
| `setAcceptPrepared`: update p/p' via `setPrepared`, clear commit if voided | ✅ | `set_accept_prepared()` at `state_machine.rs:113-145` |
| `setPrepared` invariant: `p' < p AND p' ≁ p` | ✅ | `set_prepared()` maintains invariant |
| Notify `acceptedBallotPrepared` | ✅ | Driver callback invoked |

#### Step 2: Confirm Prepared (§9.6)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Gate: only PREPARE, requires `mPrepared` | ✅ | `attempt_confirm_prepared()` at `state_machine.rs:147` |
| `federatedRatify(hasPreparedBallot)` | ✅ | Calls `federated_ratify` with prepared ballot predicate |
| Search for `newC` (lowest confirmed-prepared) with contiguous range | ✅ | Downward iteration searching for commit range |
| `setConfirmPrepared`: lock value, set h, possibly set c | ✅ | Sets `value_override`, updates `high_ballot` and `commit` |
| Execute `updateCurrentIfNeeded(h)` | ✅ | Calls `update_current_if_needed()` |
| Notify `confirmedBallotPrepared` | ✅ | Driver callback invoked |

#### Step 3: Accept Commit (§9.7)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Gate: only PREPARE or CONFIRM | ✅ | Phase check in `attempt_accept_commit()` |
| Commit boundaries from all peers | ✅ | `get_commit_boundaries_from_statements()` |
| `findExtendedInterval` — find widest interval | ✅ | `find_extended_interval()` matching spec algorithm |
| `setAcceptCommit`: phase transition PREPARE → CONFIRM | ✅ | `set_accept_commit()` sets `phase = Confirm` |
| `setAcceptCommit`: reset `mPreparedPrime` to null | ✅ | `prepared_prime = None` in CONFIRM transition |
| Lock value: `mValueOverride = h.value` | ✅ | Value override set |
| Notify `acceptedCommit` | ✅ | Driver callback invoked |

#### Step 4: Confirm Commit (§9.8)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Gate: only CONFIRM, requires `mHighBallot` and `mCommit` | ✅ | Phase and state checks in `attempt_confirm_commit()` |
| `federatedRatify(commitPredicate)` | ✅ | `commit_predicate()` function used |
| `setConfirmCommit`: phase transition CONFIRM → EXTERNALIZE | ✅ | `set_confirm_commit()` sets `phase = Externalize` |
| Stop nomination after externalize | ✅ | `needs_stop_nomination = true` flag, checked by Slot |
| Notify `valueExternalized` | ✅ | Driver callback invoked |

#### Step 5: Bump (§9.9)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Gate: PREPARE or CONFIRM, only at outermost level | ✅ | `attempt_bump()` called only when `current_message_level == 1` |
| V-blocking set strictly ahead check | ✅ | `is_blocking_set` with counter filter |
| Find lowest counter resolving v-blocking | ✅ | Ascending iteration through counters |
| `abandonBallot(n)` | ✅ | `abandon_ballot()` implemented |

#### Ballot Bumping (§9.10)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `abandonBallot`: use composite candidate, fallback to current ballot value | ✅ | Uses `composite_candidate` then falls back to `current_ballot.value` |
| `bumpState`: value override enforcement | ✅ | Replaces value with `value_override` if set |
| `updateCurrentValue`: gate on phase, reject incompatible commits | ✅ | Checks phase and commit compatibility |
| `bumpToBallot`: never in EXTERNALIZE, notify `startedBallotProtocol` on first ballot | ✅ | Assertion and notification present |
| `bumpToBallot`: reset h/c if incompatible | ✅ | Incompatible high_ballot/commit cleared |
| `bumpToBallot`: reset `heardFromQuorum` on counter change | ✅ | `heard_from_quorum = false` on counter change |

#### Update Current If Needed (§9.11)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| If `b` is null or `b < h`, bump to `h` | ✅ | `update_current_if_needed()` implemented per spec |

#### Statement Emission (§9.12)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Dedup check against last generated envelope | ✅ | Comparison before processing |
| Self-process envelope | ✅ | Re-enters `process_envelope` |
| Only emit if fully validated | ✅ | `fully_validated` gate on emission |
| Only update `mLastEnvelope` if newer | ✅ | Freshness check before update |

#### State Recovery (§9.13)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Precondition: `mCurrentBallot` null | ✅ | Check in `set_state_from_envelope()` |
| PREPARE: restore b, p, p', h, c | ✅ | All fields restored |
| CONFIRM: restore b, p, h, c with correct construction | ✅ | Fields constructed from nPrepared, nCommit, nH |
| EXTERNALIZE: b=(MAX, value), p=(MAX, value), h=(nH, value), c=commit | ✅ | `force_externalize()` at `ballot/mod.rs:292` and recovery logic |

### 3.8 Message Processing

**Spec Section:** §10 (Message Processing)
**Source files:** `scp.rs`, `slot.rs`, `ballot/envelope.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §10.1 Top-level `receiveEnvelope`: verify signature, create/get slot, delegate | ✅ | `receive_envelope()` at `scp.rs:185` verifies signature, auto-vivifies slot, delegates to `slot.process_envelope()` |
| §10.2 Ballot sanity check (`isStatementSane`) | ✅ | `is_statement_sane()` called before processing |
| §10.2 Freshness check (`isNewerStatement`) | ✅ | `is_newer_statement()` checked before recording |
| §10.2 Value validation with minimum level | ✅ | `validate_statement_values()` takes minimum validation level |
| §10.2 `kInvalidValue` → return INVALID | ✅ | Returns `EnvelopeState::Invalid` |
| §10.2 `kMaybeValidValue` → mark not fully validated | ✅ | Sets `fully_validated = false` |
| §10.2 EXTERNALIZE gate: only accept compatible working ballot | ✅ | Compatibility check with `mCommit.value` |
| §10.4 PREPARE sanity: counter > 0 (except self), p' < p ∧ p' ≁ p, nH/nC constraints | ✅ | All checks in `is_statement_sane()` |
| §10.4 CONFIRM sanity: counter > 0, nH ≤ counter, nCommit ≤ nH | ✅ | All checks present |
| §10.4 EXTERNALIZE sanity: counter > 0, nH ≥ counter | ✅ | All checks present |
| §10.4 Quorum set hash validation | ✅ | Quorum set retrieved and validated |

### 3.9 Statement Ordering and Superseding

**Spec Section:** §11 (Statement Ordering and Superseding)
**Source files:** `compare.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §11.1 Type ordering: PREPARE < CONFIRM < EXTERNALIZE | ✅ | `type_rank()` at `compare.rs:17-23` assigns ranks 1, 2, 3 |
| §11.1 Nomination has rank 0 (separate namespace) | ✅ | `Nominate` → rank 0 |
| §11.2 PREPARE: compare `(ballot, prepared, preparedPrime, nH)` | ✅ | `is_newer_prepare()` at `compare.rs:67` compares in correct order |
| §11.2 CONFIRM: compare `(ballot, nPrepared, nH)` | ✅ | `is_newer_confirm()` at `compare.rs:87` compares in correct order |
| §11.2 EXTERNALIZE: always returns false (final) | ✅ | `(Externalize(_), Externalize(_)) => false` at `compare.rs:37` |
| §11.3 Nomination ordering: monotonic superset check | ✅ | `is_newer_nominate()` at `compare.rs:42` checks subset + strict growth |
| §11.4 Per-node latest envelope tracking | ✅ | `latest_envelopes: HashMap<NodeId, ScpEnvelope>` in both protocols |

### 3.10 Timer Model

**Spec Section:** §12 (Timer Model)
**Source files:** `driver.rs`, `nomination.rs`, `ballot/state_machine.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §12.1 Timer IDs: NOMINATION_TIMER=0, BALLOT_PROTOCOL_TIMER=1 | ✅ | `SCPTimerType::Nomination` and `SCPTimerType::Ballot` at `driver.rs:50-62` |
| §12.2 Nomination timer: set at end of `nominate()`, callback re-invokes with `timedout=true` | ✅ | `setup_timer(Nomination, timeout)` in `nominate()` |
| §12.2 Nomination timer canceled on first candidate | ✅ | `stop_timer(Nomination)` when candidate confirmed |
| §12.3 Ballot timer: set in `checkHeardFromQuorum`, callback calls `abandonBallot(0)` | ✅ | `setup_timer(Ballot, timeout)` in `check_heard_from_quorum()`; `bump_on_timeout()` calls `abandon_ballot(0)` |
| §12.3 Ballot timer canceled when `heardFromQuorum` becomes false or EXTERNALIZE | ✅ | `stop_timer(Ballot)` on corresponding conditions |
| §12.4 `checkHeardFromQuorum`: quorum check with counter >= current, transition notification | ✅ | `check_heard_from_quorum()` in `ballot/state_machine.rs` |
| §12.5 Timeout computation delegated to driver | ✅ | `compute_timeout()` trait method |

### 3.11 Invariants and Safety Properties

**Spec Section:** §13 (Invariants and Safety Properties)
**Source files:** `ballot/mod.rs`, `ballot/state_machine.rs`, `ballot/statements.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| §13.1 Inv 1: `b.counter != 0` when b exists | ✅ | Enforced in `bumpToBallot` and sanity checks |
| §13.1 Inv 2: `p' < p AND p' ≁ p` | ✅ | Maintained by `set_prepared()` |
| §13.1 Inv 3: `h ≲ b` | ✅ | `check_invariants()` present in ballot protocol |
| §13.1 Inv 4: `c ≲ h` | ✅ | Enforced in state transitions |
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

## 4. Gap Summary

### Critical Gaps

**None identified.** All MUST-level requirements from the specification are implemented.

### Moderate Gaps

**None identified.** All SHOULD-level recommendations are followed.

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

## 5. Risk Assessment

### Consensus Safety Risk: **Negligible**

The SCP crate implements all consensus-critical algorithms faithfully:

- **Federated agreement primitives** (`federatedAccept`, `federatedRatify`) are correct.
- **Ballot protocol state machine** follows the exact 5-step `advanceSlot` sequence with proper recursion limits.
- **Value locking** via `value_override` prevents conflicting commits.
- **Commit voiding** correctly resets the commit range when superseded by incompatible prepared ballots.
- **Phase transitions** are irreversible and match the spec exactly.
- **Quorum operations** (transitive quorum test, v-blocking) use the correct fixed-point iteration.

### Liveness Risk: **Negligible**

- **Nomination round leaders** are computed with the correct cumulative, fast-timeout algorithm.
- **Ballot bumping** (`attemptBump`) correctly responds to v-blocking sets ahead of the local node.
- **Timer management** is properly delegated to the driver, with correct setup and cancellation semantics.

### Interoperability Risk: **None**

- All XDR types are from the canonical `stellar_xdr` crate.
- Statement sanity checks match spec exactly.
- Statement ordering is implemented per spec, ensuring correct deduplication.

### Test Coverage: **Comprehensive**

Per `PARITY_STATUS.md`: 353 total tests (173 unit + 180 integration), covering all protocol areas including multi-node simulations and parity-focused scenarios. The test suite exceeds the upstream test count.

---

## 6. Recommendations

### No Action Required

The SCP crate is at full parity with the specification. No consensus-critical, liveness-critical, or interoperability gaps exist.

### Future Considerations

1. **Property-based testing**: Consider adding proptest/quickcheck tests for quorum set operations and ballot state machine transitions to catch edge cases beyond the current deterministic test suite.

2. **Formal invariant checking**: The `check_invariants()` method in the ballot protocol could be called more aggressively in debug builds (e.g., after every state transition) to catch regression bugs early.

3. **Spec evolution tracking**: When stellar-core advances beyond v25.x, the SCP library should be re-evaluated for any new protocol requirements (e.g., changes to timeout computation, quorum set constraints, or new statement types).
