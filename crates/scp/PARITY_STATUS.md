# stellar-core Parity Status

**Crate**: `henyey-scp`
**Upstream**: `stellar-core/src/scp/`
**Overall Parity**: 95%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Ballot Protocol | Partial | Core protocol complete; reporting helpers still diverge |
| Nomination Protocol | Partial | Core protocol complete; node-state reporting is simpler |
| Quorum Operations | Full | isQuorum, isVBlocking, findClosestVBlocking |
| Quorum Set Validation | Full | Sanity checks, normalization, nesting limits |
| SCP Coordinator | Partial | Reporting and range-based purge API differ |
| Slot Management | Partial | Historical statement reporting is reduced |
| SCPDriver Trait | Partial | `acceptedCommit()` callback not exposed |
| Federated Agreement | Full | federatedAccept, federatedRatify |
| State Recovery | Full | setStateFromEnvelope for crash recovery |
| Timer Support | Full | Nomination and ballot timers |
| JSON/Info Reporting | Partial | Slot-centric serde output, fewer upstream knobs |
| Statement Ordering | Full | Newer-statement comparisons for all types |
| Display Formatting | Full | Node, ballot, envelope string formatting |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `SCP.h` / `SCP.cpp` | `scp.rs` | Main coordinator |
| `Slot.h` / `Slot.cpp` | `slot.rs` | Per-slot state container |
| `BallotProtocol.h` / `BallotProtocol.cpp` | `ballot/` | Ballot protocol state machine |
| `NominationProtocol.h` / `NominationProtocol.cpp` | `nomination.rs` | Nomination protocol |
| `LocalNode.h` / `LocalNode.cpp` | `quorum.rs` | Quorum operations (class dissolved) |
| `QuorumSetUtils.h` / `QuorumSetUtils.cpp` | `quorum.rs` | Sanity checks, normalization |
| `SCPDriver.h` / `SCPDriver.cpp` | `driver.rs` | Driver trait |

## Component Mapping

### scp (`scp.rs`)

Corresponds to: `SCP.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SCP()` | `SCP::new()` | Full |
| `getDriver()` | `driver()` | Full |
| `receiveEnvelope()` | `receive_envelope()` | Full |
| `nominate()` | `nominate()` | Full |
| `stopNomination()` | `stop_nomination()` | Full |
| `updateLocalQuorumSet()` | `set_local_quorum_set()` | Full |
| `getLocalQuorumSet()` | `local_quorum_set()` | Full |
| `getLocalNodeID()` | `local_node_id()` | Full |
| `getLocalNode()` | Embedded in SCP struct | Full |
| `getJsonInfo()` | `get_info()`, `get_all_slot_info()` | Partial |
| `getJsonQuorumInfo()` | `get_quorum_info()`, `get_quorum_info_for_node()` | Partial |
| `getState()` (reporting) | `get_reporting_state_with_lock()`, `get_reporting_summary()` | Full |
| `getMissingNodes()` | `get_missing_nodes()` | Full |
| `purgeSlotsOutsideRange()` | `purge_slots()` | Partial |
| `isValidator()` | `is_validator()` | Full |
| `isSlotFullyValidated()` | `is_slot_fully_validated()` | Full |
| `gotVBlocking()` | `got_v_blocking()` | Full |
| `getKnownSlotsCount()` | `slot_count()` | Full |
| `getCumulativeStatemtCount()` | `get_cumulative_statement_count()` | Full |
| `getLatestMessagesSend()` | `get_latest_messages_send()` | Full |
| `setStateFromEnvelope()` | `set_state_from_envelope()` | Full |
| `empty()` | `empty()` | Full |
| `processCurrentState()` | `get_scp_state()`, `get_entire_current_state()` | Full |
| `processSlotsAscendingFrom()` | `process_slots_ascending_from()` | Full |
| `processSlotsDescendingFrom()` | `process_slots_descending_from()` | Full |
| `getLatestMessage()` | `get_latest_message()` | Full |
| `isNewerNominationOrBallotSt()` | `is_newer_statement()` | Full |
| `getExternalizingState()` | `get_externalizing_state()` | Full |
| `getValueString()` | Via `SCPDriver::get_value_string()` | Full |
| `ballotToStr()` | `ballot_to_str()` | Full |
| `envToStr()` | `envelope_to_str()` | Full |
| `getHighestKnownSlotIndex()` | `get_highest_known_slot()` | Full |
| `getState()` (private) | `get_quorum_info_for_node()` | Full |
| `getSlot()` (protected) | Internal slot map access | Full |

### slot (`slot.rs`)

Corresponds to: `Slot.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Slot()` | `Slot::new()` | Full |
| `getSlotIndex()` | `slot_index()` | Full |
| `getSCP()` | Architectural difference (no back-ref) | Full |
| `getSCPDriver()` | Via driver parameter passing | Full |
| `getBallotProtocol()` | `ballot()`, `ballot_mut()` | Full |
| `getLatestCompositeCandidate()` | `get_latest_composite_candidate()` | Full |
| `getLatestMessagesSend()` | `get_latest_messages_send()` | Full |
| `setStateFromEnvelope()` | `set_state_from_envelope()` | Full |
| `processCurrentState()` | `process_current_state()` | Full |
| `getLatestMessage()` | `get_latest_envelope()` | Full |
| `isNewerNominationOrBallotSt()` | Via `compare::is_newer_nomination_or_ballot_st()` | Full |
| `getExternalizingState()` | `get_externalizing_state()` | Full |
| `recordStatement()` | Envelope history only | Partial |
| `processEnvelope()` | `process_envelope()` | Full |
| `abandonBallot()` | `abandon_ballot()` | Full |
| `bumpState()` | `bump_state()`, `force_bump_state()` | Full |
| `nominate()` | `nominate()` | Full |
| `stopNomination()` | `stop_nomination()` | Full |
| `getNominationLeaders()` | `get_nomination_leaders()` | Full |
| `isFullyValidated()` | `is_fully_validated()` | Full |
| `setFullyValidated()` | `set_fully_validated()` | Full |
| `getStatementCount()` | `get_statement_count()` | Full |
| `gotVBlocking()` | `got_v_blocking()` | Full |
| `getJsonInfo()` | `get_info()` | Full |
| `getJsonQuorumInfo()` | `get_quorum_info()` | Partial |
| `getState()` | `get_node_state()`, `get_all_node_states()`, `get_reporting_state()` | Full |
| `getCompanionQuorumSetHashFromStatement()` | `get_companion_quorum_set_hash()` | Full |
| `getStatementValues()` | `get_statement_values()` | Full |
| `getQuorumSetFromStatement()` | Via driver callback | Full |
| `createEnvelope()` | `create_envelope()` | Full |
| `federatedAccept()` | `federated_accept()` | Full |
| `federatedRatify()` | `federated_ratify()` | Full |
| `getLocalNode()` | Via SCP reference | Full |
| `getEntireCurrentState()` | `get_entire_current_state()` | Full |
| `maybeSetGotVBlocking()` | `maybe_set_got_v_blocking()` | Full |
| `timerIDs` enum | `SCPTimerType` enum | Full |

### ballot (`ballot/`)

Corresponds to: `BallotProtocol.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BallotProtocol()` | `BallotProtocol::new()` | Full |
| `processEnvelope()` | `process_envelope()` | Full |
| `ballotProtocolTimerExpired()` | `bump_timeout()` / `bump_on_timeout()` | Full |
| `abandonBallot()` | `abandon_ballot()` | Full |
| `bumpState(Value, bool)` | `bump()` | Full |
| `bumpState(Value, uint32)` | `bump_state()` / `bump_to_counter()` | Full |
| `getJsonInfo()` | `get_info()` | Full |
| `getState()` | `get_node_state()`, `get_reporting_state()` | Full |
| `getJsonQuorumInfo()` | Via Slot-level `get_quorum_info()` | Partial |
| `getCompanionQuorumSetHashFromStatement()` | None | None |
| `getWorkingBallot()` | `get_working_ballot()` | Full |
| `getLastMessageSend()` | `get_last_envelope()` | Full |
| `setStateFromEnvelope()` | `set_state_from_envelope()` | Full |
| `processCurrentState()` | `process_current_state()` | Full |
| `getLatestMessage()` | `get_latest_envelope()` | Full |
| `getExternalizingState()` | `get_externalizing_state()` | Full |
| `getStatementValues()` | Via internal logic | Full |
| `isNewerStatement()` (static) | `is_newer_statement()` / `is_newer_statement_pair()` | Full |
| `advanceSlot()` | `advance_slot()` | Full |
| `validateValues()` | `validate_statement_values()` | Full |
| `sendLatestEnvelope()` | `send_latest_envelope()` | Full |
| `attemptAcceptPrepared()` | `attempt_accept_prepared()` | Full |
| `setAcceptPrepared()` | `set_accept_prepared()` | Full |
| `attemptConfirmPrepared()` | `attempt_confirm_prepared()` | Full |
| `setConfirmPrepared()` | `set_confirm_prepared()` | Full |
| `attemptAcceptCommit()` | `attempt_accept_commit()` | Full |
| `setAcceptCommit()` | `set_accept_commit()` | Full |
| `attemptConfirmCommit()` | `attempt_confirm_commit()` | Full |
| `setConfirmCommit()` | `set_confirm_commit()` | Full |
| `attemptBump()` | `attempt_bump()` | Full |
| `getPrepareCandidates()` | `get_prepare_candidates()` | Full |
| `updateCurrentIfNeeded()` | `update_current_if_needed()` | Full |
| `findExtendedInterval()` | `find_extended_interval()` | Full |
| `getCommitBoundariesFromStatements()` | `get_commit_boundaries_from_statements()` | Full |
| `hasPreparedBallot()` | `has_prepared_ballot()` | Full |
| `commitPredicate()` | `commit_predicate()` | Full |
| `setPrepared()` | `set_prepared()` | Full |
| `compareBallots()` | `ballot_compare()` | Full |
| `areBallotsCompatible()` | `ballot_compatible()` | Full |
| `areBallotsLessAndIncompatible()` | `are_ballots_less_and_incompatible()` | Full |
| `areBallotsLessAndCompatible()` | `are_ballots_less_and_compatible()` | Full |
| `isNewerStatement()` (instance) | `is_newer_statement()` | Full |
| `isStatementSane()` | `is_statement_sane()` | Full |
| `recordEnvelope()` | `record_local_envelope()` | Full |
| `bumpToBallot()` | `bump_to_ballot()` | Full |
| `updateCurrentValue()` | `update_current_value()` | Full |
| `emitCurrentStateStatement()` | `emit_current_state()` | Full |
| `checkInvariants()` | `check_invariants()` | Full |
| `createStatement()` | `emit_prepare()` / `emit_confirm()` / `emit_externalize()` | Full |
| `getLocalState()` | `get_local_state()` | Full |
| `getLocalNode()` | Via driver/context parameters | Full |
| `federatedAccept()` | `federated_accept()` | Full |
| `federatedRatify()` | `federated_ratify()` | Full |
| `startBallotProtocolTimer()` | `check_heard_from_quorum()` (timer logic) | Full |
| `stopBallotProtocolTimer()` | `stop_timer()` | Full |
| `checkHeardFromQuorum()` | `check_heard_from_quorum()` | Full |
| `makeBallot()` | Direct `ScpBallot` construction | Full |
| `ballotToStr()` | `ballot_to_str()` | Full |
| `SCPPhase` enum | `BallotPhase` enum | Full |

### nomination (`nomination.rs`)

Corresponds to: `NominationProtocol.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `NominationProtocol()` | `NominationProtocol::new()` | Full |
| `isNewerStatement()` (static) | `is_newer_nomination()` | Full |
| `processEnvelope()` | `process_envelope()` | Full |
| `getStatementValues()` | Via Slot-level `get_statement_values()` | Full |
| `nominate()` | `nominate()` | Full |
| `stopNomination()` | `stop()` | Full |
| `getLeaders()` | `get_round_leaders()` | Full |
| `getLatestCompositeCandidate()` | `latest_composite()` | Full |
| `getJsonInfo()` | `get_info()` | Full |
| `getState()` | `get_node_state()`, `get_reporting_state()` | Full |
| `getLastMessageSend()` | `get_last_message_send()` | Full |
| `setStateFromEnvelope()` | `set_state_from_envelope()` | Full |
| `processCurrentState()` | `process_current_state()` | Full |
| `getLatestMessage()` | `get_latest_nomination()` | Full |
| `isNewerStatement()` (instance) | `is_newer_nomination_internal()` | Full |
| `isSubsetHelper()` | Inline in `is_newer_nomination()` | Full |
| `validateValue()` | Via `driver.validate_value()` | Full |
| `extractValidValue()` | Via `driver.extract_valid_value()` | Full |
| `isSane()` | `is_sane_statement()` | Full |
| `recordEnvelope()` | `record_local_nomination()` | Full |
| `emitNomination()` | `emit_nomination()` | Full |
| `acceptPredicate()` | Inline in `should_accept_value()` | Full |
| `applyAll()` | Inline iteration | Full |
| `updateRoundLeaders()` | `update_round_leaders()` | Full |
| `hashNode()` | `compute_hash_node()` via driver | Full |
| `hashValue()` | `hash_value()` via driver | Full |
| `getNodePriority()` | `get_node_priority()` | Full |
| `getNewValueFromNomination()` | `get_new_value_from_nomination()` | Full |

### quorum (`quorum.rs`)

Corresponds to: `LocalNode.h`, `QuorumSetUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LocalNode()` | Distributed across `SCP`, `Slot` | Full |
| `getNodeID()` | `SCP::local_node_id()` | Full |
| `updateQuorumSet()` | `SCP::set_local_quorum_set()` | Full |
| `getQuorumSet()` | `SCP::local_quorum_set()` | Full |
| `getQuorumSetHash()` | `hash_quorum_set()` | Full |
| `isValidator()` | `SCP::is_validator()` | Full |
| `getSingletonQSet()` | `singleton_quorum_set()` | Full |
| `forAllNodes()` | `get_all_nodes()` | Full |
| `isQuorumSlice(QSet, nodeSet)` | `is_quorum_slice()` | Full |
| `isVBlocking(QSet, nodeSet)` | `is_v_blocking()` | Full |
| `isVBlocking(QSet, map, filter)` | `is_v_blocking()` (adapted) | Full |
| `isQuorum()` | `is_quorum()` | Full |
| `findClosestVBlocking()` (set) | `find_closest_v_blocking()` | Full |
| `findClosestVBlocking()` (map) | `find_closest_v_blocking()` | Full |
| `toJson()` | Inline in `get_info()` / `get_quorum_info()` via serde | Full |
| `fromJson()` | Not needed (serde deserialization on info types) | Full |
| `to_string()` | Via Debug trait | Full |
| `buildSingletonQSet()` | `singleton_quorum_set()` | Full |
| `isQuorumSliceInternal()` | `is_quorum_slice()` (combined) | Full |
| `isVBlockingInternal()` | `is_blocking_set_helper()` | Full |
| `MAXIMUM_QUORUM_NESTING_LEVEL` | `MAXIMUM_QUORUM_NESTING_LEVEL` | Full |
| `isQuorumSetSane()` | `is_quorum_set_sane()` | Full |
| `normalizeQSet()` | `normalize_quorum_set()`, `normalize_quorum_set_with_remove()` | Full |

### driver (`driver.rs`)

Corresponds to: `SCPDriver.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `signEnvelope()` | `sign_envelope()` | Full |
| `getQSet()` | `get_quorum_set()`, `get_quorum_set_by_hash()` | Full |
| `emitEnvelope()` | `emit_envelope()` | Full |
| `validateValue()` | `validate_value()` | Full |
| `extractValidValue()` | `extract_valid_value()` | Full |
| `getValueString()` | `get_value_string()` | Full |
| `toStrKey()` | Via `node_id_to_strkey()` in `quorum_config.rs` | Full |
| `toShortString()` | `node_id_to_short_string()` in `format.rs` | Full |
| `getHashOf()` | `get_hash_of()` | Full |
| `computeHashNode()` | `compute_hash_node()` | Full |
| `computeValueHash()` | `compute_value_hash()` | Full |
| `combineCandidates()` | `combine_candidates()` | Full |
| `setupTimer()` | `setup_timer()` | Full |
| `stopTimer()` | `stop_timer()` | Full |
| `computeTimeout()` | `compute_timeout()` | Full |
| `getNodeWeight()` | `get_node_weight()`, `base_get_node_weight()` | Full |
| `computeWeight()` (helper) | `compute_weight()` | Full |
| `valueExternalized()` | `value_externalized()` | Full |
| `nominatingValue()` | `nominating_value()` | Full |
| `updatedCandidateValue()` | `updated_candidate_value()` | Full |
| `startedBallotProtocol()` | `started_ballot_protocol()` | Full |
| `acceptedBallotPrepared()` | `accepted_ballot_prepared()` | Full |
| `confirmedBallotPrepared()` | `confirmed_ballot_prepared()` | Full |
| `acceptedCommit()` | None | None |
| `ballotDidHearFromQuorum()` | `ballot_did_hear_from_quorum()` | Full |
| `ValidationLevel` enum | `ValidationLevel` enum | Extended |

**Extension note (issues #1795 and #1798):** henyey's `ValidationLevel`
adds a fourth variant `MaybeValidDeferred` that has no upstream
equivalent. The variant covers two related divergences where henyey's
fast-path forwards an SCP envelope to the ballot protocol earlier than
stellar-core's `PendingEnvelopes` / `processSCPQueue` would:

1. **Missing tx_set for LCL+1 (#1795).** Stellar-core's
   `PendingEnvelopes` buffers EXTERNALIZE envelopes until the tx_set
   arrives, so `validateValueAgainstLocalState` never sees that case.
   Henyey forwards EXTERNALIZE to SCP before the tx_set arrives (to
   allow tracking advance during catchup).
2. **Future slot while apply lags SCP (#1798).** Henyey's
   `advance_tracking_slot` runs `drain_and_process_pending` synchronously
   on the SCP externalize callback, ahead of ledger apply. Peer
   envelopes for the new tracking slot drain through while
   `lcl_seq + 1 < slot_index == tracking_index`. Stellar-core's
   `safelyProcessSCPQueue` defers the drain to the main thread via
   `postOnMainThread` (`HerderImpl.cpp:1194`).

In both cases henyey uses `MaybeValidDeferred` to signal "we could not
fully validate because of a fast-path divergence — do NOT clear
`Slot::fully_validated`". The variant behaves identically to
`MaybeValid` for all other SCP state transitions; the single behavioral
difference is gated through `ValidationLevel::clears_fully_validated()`.

**No re-validation after tx_set arrival (#1796):** `ValidationLevel` is
ephemeral — computed on-the-fly by `validate_statement_values`, used for
the `Invalid` rejection check and the `clears_fully_validated()` gate,
then discarded. No per-envelope validation state is stored in SCP.
When the tx_set arrives after a `MaybeValidDeferred` EXTERNALIZE, there
is no stored verdict to "upgrade" to `FullyValidated`. Re-feeding the
same EXTERNALIZE to SCP would be rejected by `is_stale_ballot_statement`.
Since both `MaybeValidDeferred` and `FullyValidated` return `false` from
`clears_fully_validated()`, the slot end states (externalized,
`fully_validated=true`, emission visibility) are identical.

| `hashHelper()` (private) | Inlined in `compute_hash_node()` / `compute_value_hash()` | Full |

### compare (`compare.rs`)

Corresponds to: Statement comparison logic from `Slot.h`, `BallotProtocol.h`, `NominationProtocol.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Slot::isNewerNominationOrBallotSt()` | `is_newer_nomination_or_ballot_st()` | Full |
| `NominationProtocol::isNewerStatement()` (static) | `is_newer_nominate()` | Full |
| Prepare ordering logic | `is_newer_prepare()` | Full |
| Confirm ordering logic | `is_newer_confirm()` | Full |

### format (`format.rs`)

Corresponds to: String formatting from `SCPDriver.h`, `SCP.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `toShortString()` | `node_id_to_short_string()` | Full |
| `toStrKey()` | `node_id_to_string()` | Full |
| `ballotToStr()` | `ballot_to_str()` | Full |
| `envToStr()` | `envelope_to_str()` | Full |
| Value formatting | `value_to_str()` | Full |

### info (`info.rs`)

Corresponds to: JSON info structures from `SCP.h`, `Slot.h`, `BallotProtocol.h`, `NominationProtocol.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getJsonInfo()` output | `SlotInfo`, `NominationInfo`, `BallotInfo` | Full |
| `getJsonQuorumInfo()` output | `QuorumInfo`, `NodeInfo` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `ValueWrapper` class | Rust ownership model; owned `Value` types used directly |
| `SCPEnvelopeWrapper` class | Rust ownership model; owned `ScpEnvelope` types used directly |
| `SCPBallotWrapper` class | Rust ownership model; owned `ScpBallot` used directly |
| `ValueWrapperPtr` / `shared_ptr` | Rust uses `Value` with `Clone`; no shared pointer needed |
| `SCPEnvelopeWrapperPtr` / `shared_ptr` | Rust uses `ScpEnvelope` with `Clone`; no shared pointer needed |
| `WrappedValuePtrComparator` | Standard `Ord` trait used on `Value` instead |
| `ValueWrapperPtrSet` | `Vec<Value>` with sorting used instead |
| `wrapEnvelope()` factory | Not needed; envelopes constructed directly |
| `wrapValue()` factory | Not needed; values constructed directly |
| `TestSCP` friend class | Test infrastructure; Rust tests use public API |
| `setPriorityLookup()` / `mPriorityLookupForTesting` | `BUILD_TESTS`-gated in upstream; test-only hook |
| `Node` class | Not referenced in SCP library headers; unused |

## Gaps

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `SCP::getJsonInfo()` | Medium | No aggregate `limit` / `fullKeys` API; Rust reports per-slot structs |
| `SCP::getJsonQuorumInfo()` | Medium | No upstream-style `summary` / `fullKeys` / latest-slot form |
| `SCP::purgeSlotsOutsideRange()` | Medium | Only max-bound purging is exposed |
| `Slot::recordStatement()` | Low | Historical statements lack timestamp / validated metadata |
| `BallotProtocol::getJsonQuorumInfo()` | Low | Only slot-level quorum reporting exists |
| `BallotProtocol::getCompanionQuorumSetHashFromStatement()` | Low | Helper is not exposed as a public Rust API |
| `SCPDriver::acceptedCommit()` | Low | No callback when a commit is accepted |

## Architectural Differences

1. **LocalNode dissolution**
   - **stellar-core**: `LocalNode` is a separate class holding node ID, quorum set, validator flag, and quorum operations.
   - **Rust**: Node identity fields are stored directly on the `SCP` struct. Quorum operations (isQuorum, isVBlocking, etc.) are free functions in `quorum.rs`.
   - **Rationale**: Avoids `shared_ptr<LocalNode>` indirection. Free functions are more natural in Rust and easier to test in isolation.

2. **Back-references eliminated**
   - **stellar-core**: `BallotProtocol` and `NominationProtocol` hold `Slot&` references; `Slot` holds `SCP&` reference. This creates a chain of back-references.
   - **Rust**: Protocols receive driver and context via method parameters (`SlotContext`). No stored back-references.
   - **Rationale**: Rust's borrow checker disallows shared mutable references. Passing context as parameters is idiomatic and avoids `RefCell`/`Rc` complexity.

3. **Wrapper types replaced by ownership**
   - **stellar-core**: Uses `ValueWrapper`, `SCPEnvelopeWrapper`, `SCPBallotWrapper` with `shared_ptr` for reference counting and deduplication.
   - **Rust**: Uses owned types (`Value`, `ScpEnvelope`, `ScpBallot`) directly with `Clone` when needed.
   - **Rationale**: Rust's ownership model and `Clone` trait handle value lifetimes without wrapper indirection. The performance cost of cloning XDR values is negligible compared to consensus round-trip times.

4. **JSON reporting via serde**
   - **stellar-core**: Exposes aggregate and node-targeted `Json::Value` helpers with `summary`, `fullKeys`, and latest-slot defaults.
   - **Rust**: Uses serde-derived structs (`SlotInfo`, `BallotInfo`, `NominationInfo`, `QuorumInfo`) and separate slot/node accessors.
   - **Rationale**: Type-safe serialization is simpler to maintain, but the surface is not yet API-compatible with upstream diagnostics.

5. **Error handling**
   - **stellar-core**: Uses `releaseAssert()` for invariant violations and return values for flow control.
   - **Rust**: Uses `Result` types, `Option`, and `tracing` for error reporting. Invariant violations are logged rather than panicking.
   - **Rationale**: Graceful degradation instead of process abort. Allows the node to continue operating even if a single slot encounters an anomaly.

6. **Historical slot reporting**
   - **stellar-core**: Tracks `HistoricalStatement` records with timestamps and validation markers for debug output.
   - **Rust**: Stores received envelopes per node and derives info views from current protocol state.
   - **Rationale**: Keeps slot state smaller and consensus-focused, but leaves some upstream debug detail unimplemented.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Quorum set validation | 1 TEST_CASE / 14 SECTION (`QuorumSetTests.cpp`) | 38 #[test] in `quorum.rs` | Comprehensive coverage |
| Nomination weight/stats | 3 TEST_CASE / 10 SECTION (`SCPUnitTests.cpp`) | 25 #[test] in `nomination.rs` | Priority and hash tests |
| Ballot protocol | 2 TEST_CASE / ~130 SECTION (`SCPTests.cpp`) | 55 #[test] in `ballot/` | All state transitions |
| Nomination protocol | 1 TEST_CASE / ~35 SECTION (`SCPTests.cpp`) | 25 #[test] in `nomination.rs` | Nomination flow |
| V-blocking/quorum | 2 TEST_CASE / ~35 SECTION (`SCPTests.cpp`) | 38 #[test] in `quorum.rs` | Quorum operations |
| Multi-node simulation | Covered within SCPTests.cpp | 50 #[test] in `tests/multi_node_simulation.rs` | End-to-end scenarios |
| SCP parity tests | N/A | 124 #[test] in `tests/scp_parity_tests/` | Parity-focused scenarios |
| Quorum intersection | N/A | 7 #[test] in `tests/quorum_intersection_json.rs` + 15 #[test] in `quorum_intersection/` modules | JSON-based quorum tests + SCC/MinQuorumEnumerator unit tests |
| Statement ordering | Covered within SCPTests.cpp | 3 #[test] in `compare.rs` | Statement comparison |
| Display formatting | N/A | 4 #[test] in `format.rs` | String formatting |
| Info/JSON types | N/A | 4 #[test] in `info.rs` | Serialization tests |
| Slot management | Covered within SCPTests.cpp | 26 #[test] in `slot.rs` | Per-slot operations |
| SCP coordinator | Covered within SCPTests.cpp | 15 #[test] in `scp.rs` | Top-level API tests |
| Driver | N/A | 11 #[test] in `driver.rs` | Weight computation, timeout |
| Quorum config | N/A | 6 #[test] in `quorum_config.rs` | Config parsing |
| Lib-level helpers | N/A | 2 #[test] in `lib.rs` | EnvelopeState, QuorumInfoNodeState |

**Totals**: Upstream: 9 TEST_CASE / 189 SECTION. Rust: 189 unit #[test] + 180 integration #[test] = 369 total.

### Test Gaps

No major consensus-algorithm test gaps are apparent. Remaining parity gaps are concentrated in reporting / helper APIs rather than the nomination or ballot state machines.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 206 |
| Gaps (None + Partial) | 11 |
| Intentional Omissions | 12 |
| **Parity** | **206 / (206 + 11) = 95%** |
