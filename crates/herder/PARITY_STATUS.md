# stellar-core Parity Status

**Crate**: `henyey-herder`
**Upstream**: `stellar-core/src/herder/`
**Overall Parity**: 79%
**Last Updated**: 2026-04-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Core Herder (state machine, envelope recv) | Partial | Missing metrics, quorum map reanalysis |
| HerderSCPDriver (value validation, signing) | Partial | Missing SCP metrics, node weight, TxSet validity cache; fee+account checks now wired |
| HerderPersistence (SCP state DB) | Partial | Missing `copySCPHistoryToStream`, `getNodeQuorumSet` |
| HerderUtils (value extraction) | Partial | Missing validated hash/quorum-map helpers |
| LedgerCloseData | Full | All accessors and XDR round-trip |
| PendingEnvelopes (fetching, caching) | Partial | Missing cost tracking, value size cache; release-up-to drain now matches `processSCPQueueUpToIndex`; intra-slot LIFO ordering now matches `pop()` |
| QuorumTracker | Full | expand, rebuild, closest validators |
| TransactionQueue | Partial | Arb flood damping implemented with ban-on-damping; missing dedicated flood queue |
| TxQueueLimiter | Partial | `visitTopTxs` custom limits implemented; missing total flood resource tracking |
| TxSetFrame / ApplicableTxSetFrame | Partial | No ApplicableTxSetFrame abstraction |
| SurgePricingUtils | Full | All lane configs and priority queue; `erase()` guard matches `releaseAssert(res <= mLaneCurrentCount[lane])` |
| Upgrades / ConfigUpgradeSetFrame | Partial | `isConsistentWith`, `encodeAsString`, `toJson` not implemented; proposal gating via `ConfigUpgradeContext` is now at parity |
| QuorumIntersectionChecker | Full | SCC decomposition + MinQuorumEnumerator with interrupt support |
| ParallelTxSetBuilder | Full | Implemented in parallel_tx_set_builder.rs |
| FilteredEntries | None | Not implemented (trivial) |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Herder.h` / `Herder.cpp` | `herder.rs`, `state.rs` | Interface + state enum |
| `HerderImpl.h` / `HerderImpl.cpp` | `herder.rs` | Main implementation |
| `HerderSCPDriver.h` / `HerderSCPDriver.cpp` | `scp_driver.rs` | SCP callbacks |
| `HerderPersistence.h` / `HerderPersistenceImpl.h` / `HerderPersistenceImpl.cpp` | `persistence.rs` | SCP state persistence |
| `HerderUtils.h` / `HerderUtils.cpp` | `herder_utils.rs` | Utility functions |
| `LedgerCloseData.h` / `LedgerCloseData.cpp` | `ledger_close_data.rs` | Ledger close wrapper |
| `PendingEnvelopes.h` / `PendingEnvelopes.cpp` | `pending.rs`, `fetching_envelopes.rs` | Split into two modules |
| `QuorumTracker.h` / `QuorumTracker.cpp` | `quorum_tracker.rs` | Transitive quorum tracking |
| `TransactionQueue.h` / `TransactionQueue.cpp` | `tx_queue/mod.rs`, `tx_queue/selection.rs` | Queue and selection; `broadcast_with_visitor()` on `TransactionQueue` |
| `TxQueueLimiter.h` / `TxQueueLimiter.cpp` | `tx_queue_limiter.rs` | Resource-aware limiting |
| `TxSetFrame.h` / `TxSetFrame.cpp` | `tx_queue/tx_set.rs` | Simplified; no ApplicableTxSetFrame |
| `TxSetUtils.h` / `TxSetUtils.cpp` | `tx_set_utils.rs` | Filtering and validation utilities |
| `SurgePricingUtils.h` / `SurgePricingUtils.cpp` | `surge_pricing.rs` | Lane configs + priority queue |
| `Upgrades.h` / `Upgrades.cpp` | `upgrades.rs` | Upgrade scheduling |
| `ParallelTxSetBuilder.h` / `ParallelTxSetBuilder.cpp` | `parallel_tx_set_builder.rs` | Parallel Soroban phase |
| `FilteredEntries.h` | _(not implemented)_ | Trivial constant array |

## Component Mapping

### Herder (`herder.rs`, `state.rs`)

Corresponds to: `Herder.h`, `HerderImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Herder::State` enum | `HerderState` enum | Full |
| `Herder::EnvelopeStatus` enum | `EnvelopeState` enum | Full |
| `getState()` | `state()` | Full |
| `getStateHuman()` | `HerderState::Display` | Full |
| `syncMetrics()` | _(not implemented)_ | None |
| `bootstrap()` | `bootstrap()` | Full |
| `shutdown()` | _(not implemented)_ | None |
| `start()` | _(not implemented)_ | None |
| `lastClosedLedgerIncreased()` | `ledger_closed()` | Full |
| `setTrackingSCPState()` | `advance_tracking_slot()` | Full |
| `recvSCPQuorumSet()` | `store_quorum_set()` | Full |
| `recvTxSet()` | `receive_tx_set()` | Full |
| `recvTransaction()` | `receive_transaction()` | Full |
| `peerDoesntHave()` | `fetching_envelopes.peer_doesnt_have()` | Full |
| `getTxSet()` | `scp_driver.get_tx_set()` | Full |
| `getQSet()` | `get_quorum_set_by_hash()` | Full |
| `recvSCPEnvelope()` | `receive_scp_envelope()` | Full |
| `isTracking()` | `is_tracking()` | Full |
| `sendSCPStateToPeer()` | `get_scp_state()` | Full |
| `trackingConsensusLedgerIndex()` | `tracking_slot()` | Full |
| `getMaxClassicTxSize()` | via `flow_control` module | Full |
| `getMaxTxSize()` | via `flow_control` module | Full |
| `getFlowControlExtraBuffer()` | `FLOW_CONTROL_BYTES_EXTRA_BUFFER` | Full |
| `getMinLedgerSeqToAskPeers()` | `get_min_ledger_seq_to_ask_peers()` | Full |
| `getMinLedgerSeqToRemember()` | `get_min_ledger_seq_to_remember()` | Full |
| `isNewerNominationOrBallotSt()` | _(not implemented)_ | None |
| `getMostRecentCheckpointSeq()` | `get_most_recent_checkpoint_seq()` | Full |
| `triggerNextLedger()` | `trigger_next_ledger()` | Full | Henyey adds an `is_nominating` idempotency guard that skips duplicate triggers while nomination is active. stellar-core logs and continues on duplicate calls but never invokes them in a retry loop. Observable SCP behavior is unchanged. |
| Nomination value caching (timer lambda capture) | `cached_nomination_value` field + `handle_nomination_timeout()` | Full |
| `setInSyncAndTriggerNextLedger()` | `trigger_next_ledger()` | Full |
| `resolveNodeID()` | _(not implemented)_ | None |
| `setUpgrades()` | _(not implemented)_ | None |
| `getUpgradesJson()` | _(not implemented)_ | None |
| `setFilteredAccounts()` | _(not implemented)_ | None |
| `forceSCPStateIntoSyncWithLastClosedLedger()` | EXTERNALIZE flows through `scp.receive_envelope()` | Full |
| `makeStellarValue()` | `scp_driver.make_stellar_value()` | Full |
| `getJsonInfo()` | `json_api.rs` structures | Partial |
| `getJsonQuorumInfo()` | `json_api.rs` structures | Partial |
| `getJsonTransitiveQuorumInfo()` | `json_api.rs` structures | Partial |
| `getCurrentlyTrackedQuorum()` | `quorum_tracker.quorum_map()` | Full |
| `getMaxQueueSizeOps()` | `max_queue_size_ops()` | Full |
| `getMaxQueueSizeSorobanOps()` | `update_soroban_resource_limits()` / `effective_queue_soroban_resources()` | Full |
| `maybeHandleUpgrade()` | Post-close upgrade check in `henyey-app` | Full |
| `isBannedTx()` | `tx_queue.is_banned()` | Full |
| `getTx()` | `tx_queue.get_tx()` | Full |
| `processExternalized()` | handled in `receive_scp_envelope()` | Full |
| `valueExternalized()` | handled in `receive_scp_envelope()` | Full |
| `emitEnvelope()` | handled by `ScpDriver` | Full |
| `lostSync()` | `SyncRecoveryManager::record_lost_sync()` | Full |
| `checkCloseTime()` | `check_envelope_close_time()` | Full |
| `ctValidityOffset()` | _(not implemented)_ | None |
| `setupTriggerNextLedger()` | _(not implemented)_ | None |
| `startOutOfSyncTimer()` | `SyncRecoveryManager` | Full |
| `outOfSyncRecovery()` | `out_of_sync_recovery()` | Full |
| `broadcast()` | `flush_tx_adverts()` in `App` | Partial — priority-ordered via `TransactionQueue::broadcast_with_visitor()` with DEX-lane flood budget, budget-neutral skipped txs, arb flood damping, and ban-on-damping; broadcast period uses `flood_tx_period_ms` (200 ms) matching stellar-core `FLOOD_TX_PERIOD_MS`; missing dedicated flood queue, mark-on-attempt, separate advert flush timer |
| `processSCPQueue()` | pending envelope release | Full |
| `updateTransactionQueue()` | handled in `ledger_closed()` | Full |
| `maybeSetupSorobanQueue()` | Integrated via lane-based `TransactionQueue` | Full |
| `herderOutOfSync()` | `SyncRecoveryManager` | Full |
| `getMoreSCPState()` | _(not implemented)_ | None |
| `persistSCPState()` | `ScpPersistenceManager.persist()` | Full |
| `restoreSCPState()` | `ScpPersistenceManager.restore()` | Full |
| `persistUpgrades()` | `UpgradeParameters` with Serde persistence | Full |
| `restoreUpgrades()` | `UpgradeParameters` with Serde persistence | Full |
| `trackingHeartBeat()` | `SyncRecoveryManager` | Full |
| `startCheckForDeadNodesInterval()` | `DeadNodeTracker` | Full |
| `checkAndMaybeReanalyzeQuorumMap()` | _(not implemented)_ | None |
| `checkAndMaybeReanalyzeQuorumMapV2()` | _(not implemented)_ | None |
| `eraseBelow()` | `fetching_envelopes.erase_below()` | Full |
| `verifyEnvelope()` | `scp_driver.verify_envelope()` | Full |
| `signEnvelope()` | `scp_driver.sign_envelope()` | Full |
| `verifyStellarValueSignature()` | `scp_driver.verify_stellar_value_signature()` | Full |
| `startTxSetGCTimer()` | _(handled differently)_ | None |
| `recomputeKeysToFilter()` | _(not implemented)_ | None |

### SCP Driver (`scp_driver.rs`)

Corresponds to: `HerderSCPDriver.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `bootstrap()` | handled in Herder | Full |
| `stateChanged()` | _(not implemented)_ | None |
| `getSCP()` | via `Herder.scp` field | Full |
| `recordSCPExecutionMetrics()` | _(not implemented)_ | None |
| `recordSCPEvent()` | _(not implemented)_ | None |
| `recordSCPExternalizeEvent()` | _(not implemented)_ | None |
| `wrapEnvelope()` | _(handled differently)_ | None |
| `signEnvelope()` | `sign_envelope()` | Full |
| `emitEnvelope()` | `emit_envelope()` | Full |
| `validateValue()` | `validate_value()` | Extended |
| `extractValidValue()` | `extract_valid_value()` | Full |
| `toShortString()` | `to_short_string()` | Full |
| `getValueString()` | `get_value_string()` | Full |
| `setupTimer()` | `TimerManager` | Full |
| `stopTimer()` | `TimerManager` | Full |
| `computeTimeout()` | `compute_timeout()` | Full |
| `getHashOf()` | `compute_hash_node()` | Full |
| `combineCandidates()` | `combine_candidates()` | Full |
| `valueExternalized()` | `record_externalized()` | Full |
| `hasUpgrades()` | `has_upgrades()` | Full |
| `stripAllUpgrades()` | `strip_all_upgrades()` | Full |
| `getUpgradeNominationTimeoutLimit()` | `get_upgrade_nomination_timeout_limit()` | Partial |
| `nominate()` | handled in Herder | Full |
| `getQSet()` | `get_quorum_set_by_hash()` | Full |
| `ballotDidHearFromQuorum()` | _(not implemented)_ | None |
| `nominatingValue()` | _(not implemented)_ | None |
| `updatedCandidateValue()` | _(not implemented)_ | None |
| `startedBallotProtocol()` | _(not implemented)_ | None |
| `acceptedBallotPrepared()` | _(not implemented)_ | None |
| `confirmedBallotPrepared()` | _(not implemented)_ | None |
| `acceptedCommit()` | _(not implemented)_ | None |
| `getPrepareStart()` | _(not implemented)_ | None |
| `toStellarValue()` | `parse_stellar_value()` | Full |
| `checkCloseTime()` | `check_close_time()` | Full |
| `wrapStellarValue()` | _(not implemented)_ | None |
| `wrapValue()` | _(not implemented)_ | None |
| `purgeSlots()` | `purge_slots_below()` | Full |
| `getExternalizeLag()` | _(not implemented)_ | None |
| `getQsetLagInfo()` | _(not implemented)_ | None |
| `getMaybeDeadNodes()` | `DeadNodeTracker` | Full |
| `startCheckForDeadNodesInterval()` | `DeadNodeTracker` | Full |
| `getNodeWeight()` | _(not implemented)_ | None |
| `cacheValidTxSet()` | _(not implemented)_ | None |
| `checkAndCacheTxSetValid()` | _(not implemented)_ | None |

### Persistence (`persistence.rs`)

Corresponds to: `HerderPersistence.h`, `HerderPersistenceImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `saveSCPHistory()` | `ScpPersistenceManager.persist()` | Full |
| `copySCPHistoryToStream()` | _(not implemented)_ | None |
| `getNodeQuorumSet()` | _(not implemented)_ | None |
| `getQuorumSet()` | _(not implemented)_ | None |
| `dropAll()` | _(not implemented)_ | None |
| `deleteOldEntries()` | `delete_scp_state_below()` | Full |

### HerderUtils (`herder_utils.rs`)

Corresponds to: `HerderUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `toStellarValue()` | `ScpDriver::parse_stellar_value()` | Full |
| `getTxSetHashes()` | `get_tx_set_hashes_from_envelope()` | Full |
| `getValidatedTxSetHashes()` | _(not implemented)_ | None |
| `getStellarValues()` | `get_stellar_values()` | Full |
| `toShortString()` | `to_short_string()`, `to_short_strkey()` | Full |
| `toQuorumIntersectionMap()` | _(not implemented)_ | None |
| `parseQuorumMapFromJson()` | _(not implemented)_ | None |

### LedgerCloseData (`ledger_close_data.rs`)

Corresponds to: `LedgerCloseData.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerCloseData()` constructor | `LedgerCloseData::new()` | Full |
| `getLedgerSeq()` | `ledger_seq()` | Full |
| `getTxSet()` | `tx_set()` | Full |
| `getValue()` | `value()` | Full |
| `getExpectedHash()` | `expected_hash()` | Full |
| `toXDR()` | `to_xdr()` | Full |
| `toLedgerCloseData()` | `from_xdr()` | Full |
| `stellarValueToString()` | `stellar_value_to_string()` | Full |

### PendingEnvelopes (`pending.rs`, `fetching_envelopes.rs`)

Corresponds to: `PendingEnvelopes.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `recvSCPEnvelope()` | `FetchingEnvelopes::recv_envelope()` | Full |
| `addSCPQuorumSet()` | `FetchingEnvelopes::recv_quorum_set()` | Full |
| `recvSCPQuorumSet()` | `FetchingEnvelopes::recv_quorum_set()` | Full |
| `addTxSet()` | `FetchingEnvelopes::recv_tx_set()` | Full |
| `putTxSet()` | `FetchingEnvelopes::cache_tx_set()` | Full |
| `recvTxSet()` | `FetchingEnvelopes::recv_tx_set()` | Full |
| `peerDoesntHave()` | `FetchingEnvelopes::peer_doesnt_have()` | Full |
| `pop()` | `FetchingEnvelopes::pop()` | Full |
| `eraseBelow()` | `FetchingEnvelopes::erase_below()` + `QuorumSetTracker::evict_pending_below()` | Full |
| `stopAllOutsideRange()` (lower bound) | `PendingEnvelopes::purge_slots_below()` | Full |
| `forceRebuildQuorum()` | _(not implemented)_ | None |
| `readySlots()` | `FetchingEnvelopes::ready_slots()` | Full |
| `getJsonInfo()` | _(not implemented)_ | None |
| `getTxSet()` | `FetchingEnvelopes::get_tx_set()` | Full |
| `getQSet()` | `FetchingEnvelopes::get_quorum_set()` | Full |
| `isNodeDefinitelyInQuorum()` | `QuorumTracker::is_node_definitely_in_quorum()` | Full |
| `rebuildQuorumTrackerState()` | _(not implemented)_ | None |
| `getCurrentlyTrackedQuorum()` | `QuorumTracker::quorum_map()` | Full |
| `envelopeProcessed()` | handled in pop/processing | Full |
| `reportCostOutliersForSlot()` | _(not implemented)_ | None |
| `getJsonValidatorCost()` | _(not implemented)_ | None |
| `recordReceivedCost()` | _(not implemented)_ | None |
| `getCostPerValidator()` | _(not implemented)_ | None |

**Parity notes (pending.rs):**
- **No per-slot envelope count cap** (matches stellar-core). Removed the
  henyey-specific `max_per_slot` field in #1899 — stellar-core's
  `PendingEnvelopes` uses an unbounded `std::map<uint64, SlotEnvelopes>`
  with no per-slot limit. Cleanup is slot-based, not per-envelope.
- **Slot-count gating** correctly skips the `max_slots` check when appending
  to an existing slot (fixed in #1899).
- **`purge_slots_below(min_slot)`** mirrors the lower-bound cleanup of
  stellar-core's `stopAllOutsideRange`. Wired into `out_of_sync_recovery`
  and `ledger_closed`.
- **Follow-up:** Full `purge_slots_outside_range(min, max, slot_to_keep)`
  with upper bound and checkpoint preservation is not yet implemented.
- **Intra-slot release order** now matches stellar-core's LIFO semantics
  (fixed in #1969). `release()` reverses the insertion-order vec so
  last-added envelopes are processed first, matching `pop()`'s
  `v.back()` / `pop_back()` behavior.

### QuorumTracker (`quorum_tracker.rs`)

Corresponds to: `QuorumTracker.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `QuorumTracker()` constructor | `QuorumTracker::new()` | Full |
| `isNodeDefinitelyInQuorum()` | `is_node_definitely_in_quorum()` | Full |
| `expand()` | `expand()` | Full |
| `rebuild()` | `rebuild()` | Full |
| `getQuorum()` | `quorum_map()` | Full |
| `findClosestValidators()` | `find_closest_validators()` | Full |

### TransactionQueue (`tx_queue.rs`)

Corresponds to: `TransactionQueue.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `tryAdd()` | `try_add()` | Full |
| `removeApplied()` | `remove_applied()` | Full |
| `ban()` | `ban()` | Full |
| `shift()` | `shift()` | Full |
| `rebroadcast()` | _(removed — replaced by periodic `broadcast_with_visitor()`)_ | N/A |
| `shutdown()` | _(not implemented)_ | None |
| `isBanned()` | `is_banned()` | Full |
| `getTx()` | `get_tx()` | Full |
| `getTransactions()` | `get_transactions()` | Full |
| `sourceAccountPending()` | `Herder::source_account_pending()` | Full |
| `getMaxQueueSizeOps()` | via config | Full |
| `findAllAssetPairsInvolvedInPaymentLoops()` | _(not implemented)_ | None |
| `canAdd()` | logic in `try_add()` | Full |
| `releaseFeeMaybeEraseAccountState()` | `drop_transaction()` | Full |
| `prepareDropTransaction()` | _(not needed — Rust ownership)_ | _(omitted)_ |
| `dropTransaction()` | `drop_transaction()` | Full |
| `isFiltered()` | `is_filtered()` | Full |
| `broadcastTx()` | _(removed — replaced by `broadcast_with_visitor()`)_ | N/A |
| `broadcastSome()` | `TransactionQueue::broadcast_with_visitor()` | Partial — priority-ordered with ops budget, DEX-lane limits, budget-neutral skipped txs, arb flood damping with ban-on-damping; missing dedicated flood queue |
| `SorobanTransactionQueue::resetAndRebuild()` | `reset_and_rebuild()` in `tx_queue.rs` | Full |
| `SorobanTransactionQueue::getMaxQueueSizeOps()` | via config | Full |
| `ClassicTransactionQueue::getMaxQueueSizeOps()` | via config | Full |
| `ClassicTransactionQueue::allowTxBroadcast()` | `ArbitrageFloodDamper::allow_tx_broadcast()` | Full |

### TxQueueLimiter (`tx_queue_limiter.rs`)

Corresponds to: `TxQueueLimiter.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `TxQueueLimiter()` constructor | `TxQueueLimiter::new()` | Full |
| `addTransaction()` | `add_transaction()` | Full |
| `removeTransaction()` | `remove_transaction()` | Full |
| `maxScaledLedgerResources()` | _(removed)_ | None |
| `evictTransactions()` | `evict_transactions()` | Full |
| `canAddTx()` | `can_add_tx()` | Full |
| `resetEvictionState()` | `reset_eviction_state()` | Full |
| `reset()` | `reset()` | Full |
| `visitTopTxs()` | `visit_top_txs()` | Full |
| `getTotalResourcesToFlood()` | _(not implemented)_ | None |
| `resetBestFeeTxs()` | `reset_best_fee_txs()` | Full |
| `markTxForFlood()` | `mark_tx_for_flood()` | Full |

### SurgePricingUtils (`surge_pricing.rs`)

Corresponds to: `SurgePricingUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `feeRate3WayCompare()` | `fee_rate_cmp()` | Full |
| `computeBetterFee()` | `compute_better_fee()` | Full |
| `SurgePricingLaneConfig` (interface) | `SurgePricingLaneConfig` trait | Full |
| `DexLimitingLaneConfig` | `DexLimitingLaneConfig` | Full |
| `SorobanGenericLaneConfig` | `SorobanGenericLaneConfig` | Full |
| `SurgePricingPriorityQueue` | `SurgePricingPriorityQueue` | Full |
| `getMostTopTxsWithinLimits()` | `get_most_top_txs_within_limits()` | Full |
| `totalResources()` | `total_resources()` | Full |
| `laneResources()` | `lane_resources()` | Full |
| `visitTopTxs()` | `visit_top_txs()` | Full |
| `add()` / `erase()` | `add()` / `erase()` | Full |
| `canFitWithEviction()` | `can_fit_with_eviction()` | Full |
| `popTopTxs()` | `pop_top_txs()` | Full |

### TxSetFrame (`tx_queue.rs` TransactionSet)

Corresponds to: `TxSetFrame.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `TxSetXDRFrame::makeEmpty()` | `TransactionSet::new()` | Full |
| `TxSetXDRFrame::makeFromWire()` | `TransactionSet::from_xdr_stored_set()` | Partial |
| `TxSetXDRFrame::makeFromStoredTxSet()` | `TransactionSet::from_xdr_stored_set()` | Full |
| `TxSetXDRFrame::makeFromHistoryTransactions()` | _(not implemented)_ | None |
| `TxSetXDRFrame::toXDR()` | `TransactionSet::to_xdr_stored_set()` | Full |
| `TxSetXDRFrame::getContentsHash()` | `TransactionSet::hash` | Full |
| `TxSetXDRFrame::previousLedgerHash()` | `TransactionSet::previous_ledger_hash` | Full |
| `TxSetXDRFrame::sizeTxTotal()` | `TransactionSet::len()` | Full |
| `TxSetXDRFrame::sizeOpTotalForLogging()` | _(not implemented)_ | None |
| `TxSetXDRFrame::encodedSize()` | _(not implemented)_ | None |
| `TxSetXDRFrame::createTransactionFrames()` | _(not implemented)_ | None |
| `TxSetXDRFrame::prepareForApply()` | `TransactionSet::prepare_for_apply()` | Full |
| `ApplicableTxSetFrame::getTxBaseFee()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::getPhase()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::getPhases()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::getPhasesInApplyOrder()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::checkValid()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::size()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::getTotalFees()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::getTotalInclusionFees()` | _(not implemented)_ | None |
| `ApplicableTxSetFrame::summary()` | `TransactionSet` summary logging | Partial |
| `TxSetPhaseFrame` (all methods) | _(not implemented)_ | None |
| `makeTxSetFromTransactions()` | `build_generalized_tx_set()` | Full |

### TxSetUtils (`tx_set_utils.rs`)

Corresponds to: `TxSetUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `hashTxSorter()` | tx hash sorting in set building | Full |
| `sortTxsInHashOrder()` | handled in set building | Full |
| `sortParallelTxsInHashOrder()` | _(not implemented)_ | None |
| `buildAccountTxQueues()` | _(not implemented)_ | None |
| `getInvalidTxList()` | `get_invalid_tx_list()` in `tx_set_utils.rs` | Full |
| `trimInvalid()` | `trim_invalid()` in `tx_set_utils.rs` | Full |
| `validateTxSetXDRStructure()` | `validate_generalized_tx_set_xdr_structure()` in `tx_queue.rs` | Full |

### Upgrades (`upgrades.rs`)

Corresponds to: `Upgrades.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Upgrades()` constructor | `Upgrades::new()` | Full |
| `setParameters()` | `set_parameters()` | Full |
| `getParameters()` | `parameters()` | Full |
| `createUpgradesFor()` | `create_upgrades_for()` | Full | Now gates config upgrades via `ConfigUpgradeContext` (parity with `makeFromKey + isValidForApply + upgradeNeeded` in stellar-core) |
| `applyTo()` | _(not implemented)_ | None |
| `toString(LedgerUpgrade)` | `upgrade_to_string()` | Full |
| `isValidForApply()` | `is_valid_for_apply()` | Full |
| `isValid()` | `is_valid()` | Full |
| `toString()` | `Display` impl | Full |
| `removeUpgrades()` | `remove_upgrades()` | Full |
| `dropAll()` | _(not implemented)_ | None |
| `dropSupportUpgradeHistory()` | _(not implemented)_ | None |
| `ConfigUpgradeSetFrame::makeFromKey()` | `ConfigUpgradeSetFrame::make_from_key()` (henyey-ledger) | Full |
| `ConfigUpgradeSetFrame::getLedgerKey()` | `ConfigUpgradeSetFrame::get_ledger_key()` (henyey-ledger) | Full |
| `ConfigUpgradeSetFrame::toXDR()` | `ConfigUpgradeSetFrame::to_xdr()` (henyey-ledger) | Full |
| `ConfigUpgradeSetFrame::getKey()` | _(not needed — key stored externally)_ | N/A |
| `ConfigUpgradeSetFrame::upgradeNeeded()` | `ConfigUpgradeSetFrame::upgrade_needed()` (henyey-ledger) | Full | Added in #1952 |
| `ConfigUpgradeSetFrame::applyTo()` | `ConfigUpgradeSetFrame::apply_to()` (henyey-ledger) | Full |
| `ConfigUpgradeSetFrame::isConsistentWith()` | _(not implemented)_ | None |
| `ConfigUpgradeSetFrame::isValidForApply()` | `ConfigUpgradeSetFrame::is_valid_for_apply()` (henyey-ledger) | Full |
| `ConfigUpgradeSetFrame::encodeAsString()` | _(not implemented)_ | None |
| `ConfigUpgradeSetFrame::toJson()` | _(not implemented)_ | None |

### ParallelTxSetBuilder (`parallel_tx_set_builder.rs`)

Corresponds to: `ParallelTxSetBuilder.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `buildSurgePricedParallelSorobanPhase()` | `build_surge_priced_parallel_soroban_phase()` | Full |
| Fee-rate ordering (per-op via SurgePricingPriorityQueue) | `fee_rate_cmp` cross-multiply sort | Full — fixed #1717 |

### Selection (`tx_queue/selection.rs`)

Corresponds to: `TxSetFrame.cpp` build logic

| stellar-core | Rust | Status |
|--------------|------|--------|
| Instruction limit relaxation for parallel phases (TxSetFrame.cpp:513-524) | `build_soroban_phase()` sets Instructions to i64::MAX | Full — fixed #1710 |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `QuorumIntersectionChecker` / `QuorumIntersectionCheckerImpl` | Implemented in `henyey-scp` crate (`quorum_intersection/` module) with full SCC + MinQuorumEnumerator algorithm |
| `RustQuorumCheckerAdaptor` | C++/Rust FFI bridge for quorum checker; not applicable in pure Rust |
| `FilteredEntries.h` (`KEYS_TO_FILTER_P24`) | Empty array constant in upstream; P24 filtering not needed for P24+ |
| `Upgrades::applyTo()` (static) | Ledger upgrade application handled by `henyey-ledger` crate |
| `Upgrades::dropAll()` / `dropSupportUpgradeHistory()` | Database schema management handled by `henyey-db` crate |
| `HerderPersistence::dropAll()` | Database schema management handled by `henyey-db` crate |
| `HerderPersistence::copySCPHistoryToStream()` | History archiving handled by `henyey-history` crate |
| `ConfigUpgradeSetFrame` (entire class) | Soroban config upgrade retrieval from ledger state handled by `henyey-ledger` crate |
| `#ifdef BUILD_TESTS` methods | Test-only overrides not needed in Rust architecture |
| `Herder::create()` (factory) | Rust uses direct construction |
| `Herder::shutdown()` | Rust uses RAII; drop handles cleanup |
| medida metrics (`SCPMetrics`, `QueueMetrics`) | Metrics infrastructure uses different Rust libraries |
| `prepareDropTransaction()` | Rust ownership model doesn't need a separate "prepare" step; `drop_transaction()` handles both |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `ApplicableTxSetFrame` abstraction | Low | Validation done inline in `tx_set.rs`; functionally equivalent |
| `TxSetPhaseFrame` | Medium | Phase-level abstraction for parallel support |
| `TxSetUtils::buildAccountTxQueues()` | Low | Account-based tx ordering |
| `recordSCPExecutionMetrics()` | Low | SCP performance metrics |
| `recordSCPEvent()` / `recordSCPExternalizeEvent()` | Low | SCP event tracking |
| `getExternalizeLag()` / `getQsetLagInfo()` | Low | Externalize timing metrics |
| `getNodeWeight()` | Medium | Application-specific leader election (P22+) |
| `cacheValidTxSet()` / `checkAndCacheTxSetValid()` | Low | TxSet validity caching |
| `wrapEnvelope()` / `wrapStellarValue()` / `wrapValue()` | Low | Value wrapper pattern |
| Ballot phase callbacks (7 methods) | Low | Logging/metrics callbacks |
| `getPrepareStart()` / SCPTiming | Partial | `SlotTimingState::ballot_start` tracks prepare start; `nomination_duration` matches `mNominateToPrepare`. Missing: `mPrepareToExternalize` metric. |
| `getUpgradeNominationTimeoutLimit()` | Medium | Uses a hard-coded default instead of runtime upgrade parameters |
| `syncMetrics()` | Low | Metrics synchronization |
| `isNewerNominationOrBallotSt()` | Medium | Envelope dedup optimization |
| `resolveNodeID()` | Low | Config-based node name lookup |
| `setUpgrades()` / `getUpgradesJson()` | Medium | Admin API for upgrade scheduling |
| `setFilteredAccounts()` | Medium | Runtime filtered-account override API missing |
| `checkAndMaybeReanalyzeQuorumMap()` | Low | Background quorum analysis |
| `getMoreSCPState()` | Low | Peer SCP state request |
| `recomputeKeysToFilter()` | Low | Soroban footprint filtering |
| `ctValidityOffset()` | Low | Close time offset computation |
| PendingEnvelopes cost tracking (4 methods) | Low | Per-validator cost analysis |
| `HerderPersistence::getNodeQuorumSet()` | Low | Node-level quorum set lookup |
| `HerderPersistence::getQuorumSet()` | Low | Hash-based quorum set lookup |
| `HerderUtils::getValidatedTxSetHashes()` | Low | No strict variant that errors on malformed envelope values |
| `HerderUtils::toQuorumIntersectionMap()` | Low | Quorum map conversion |
| `HerderUtils::parseQuorumMapFromJson()` | Low | JSON quorum map parsing |
| `TxSetXDRFrame::makeFromHistoryTransactions()` | Low | History tx set construction |
| `TxSetXDRFrame::encodedSize()` | Low | Wire size calculation |
| `TxSetUtils::sortParallelTxsInHashOrder()` | Low | Parallel stage sorting |
| `maxScaledLedgerResources()` | Low | Removed during simplification; re-add if needed |
| `getTotalResourcesToFlood()` | Low | Flood resource tracking |
| `stateChanged()` | Low | SCP state change callback |
| `startTxSetGCTimer()` | Low | Tx set garbage collection |

## Architectural Differences

1. **Concurrency Model**
   - **stellar-core**: Single-threaded with `VirtualClock` timers and callback-driven processing
   - **Rust**: Thread-safe with `RwLock` and `DashMap`; async-ready with tokio integration via `TimerManager` and `SyncRecoveryManager`
   - **Rationale**: Rust's ownership model and async runtime provide thread safety without a global event loop

2. **Pending Envelopes Split**
   - **stellar-core**: Single `PendingEnvelopes` class handles both slot-based buffering and dependency fetching
   - **Rust**: Split into `PendingEnvelopes` (slot-based buffering) and `FetchingEnvelopes` (dependency fetching with `ItemFetcher`)
   - **Rationale**: Separation of concerns; each module has a single responsibility

3. **Transaction Queue Architecture**
   - **stellar-core**: Separate `ClassicTransactionQueue` and `SorobanTransactionQueue` classes using C++ inheritance
   - **Rust**: Unified `TransactionQueue` with lane-based separation via `SurgePricingLaneConfig` trait
   - **Rationale**: Rust trait-based composition avoids inheritance hierarchy while maintaining lane separation

4. **Transaction Set Abstraction**
   - **stellar-core**: `TxSetXDRFrame` (wire) -> `ApplicableTxSetFrame` (validated, apply-ready) with `TxSetPhaseFrame` per phase
   - **Rust**: Single `TransactionSet` struct with direct `GeneralizedTransactionSet` building
   - **Rationale**: Simplified for current needs; full abstraction layers can be added when needed

5. **Timer Management**
   - **stellar-core**: `VirtualTimer` instances per slot/timer ID, managed by `Application::VirtualClock`
   - **Rust**: `TimerManager` actor with tokio channels; timers are commands sent via `mpsc`
   - **Rationale**: Actor model is natural for async Rust; avoids shared mutable timer state

6. **Metrics**
   - **stellar-core**: medida library with counters, meters, timers, histograms integrated throughout
   - **Rust**: No metrics infrastructure; statistics tracked via simple structs
   - **Rationale**: Metrics will use Rust ecosystem libraries (prometheus, metrics crate) when added

7. **Fast-path + `MaybeValidDeferred`** (issues #1795, #1798, and H-014 / #2096)
   - **stellar-core**: `PendingEnvelopes` buffers peer EXTERNALIZE
     envelopes until their tx_set arrives. By the time
     `validateValueAgainstLocalState` runs, the tx_set is always
     present; missing tx_set on LCL+1 returns `kInvalidValue`
     (`HerderSCPDriver.cpp:306-311`). In parallel-apply mode,
     `safelyProcessSCPQueue` defers the SCP queue drain to the main
     thread via `postOnMainThread` (`HerderImpl.cpp:1194`), giving
     ledger apply a chance to complete before peer envelopes for the
     next tracking slot are processed. `setupTriggerNextLedger`
     (`HerderImpl.cpp:1237-1254`) further asserts that apply is not
     in flight and tracking equals the LCL — together these mean the
     henyey-specific apply-lag window does not occur upstream.
   - **Rust**: `process_scp_envelope` forwards peer EXTERNALIZE to SCP
     before the tx_set is fetched, so tracking advance can proceed
     during catchup (#1795). `advance_tracking_slot` then runs
     `drain_and_process_pending(consensus_index)` synchronously on the
     SCP externalize callback, ahead of ledger apply (#1798). In both
     windows, `validate_value_against_local_state` /
     `validate_past_or_future_value` return
     `ValueValidation::MaybeValidDeferred`, which maps to
     `ValidationLevel::MaybeValidDeferred` — a henyey extension that
     **does** clear `Slot::fully_validated` (matching stellar-core's
     `kMaybeValidValue` clear) and is restored by paired herder-side
     triggers when the deferred condition resolves.
   - **Restoration triggers**: `MaybeValidDeferred` registers each
     deferred slot in `ScpDriver::deferred_slots` keyed by
     `DeferredCauses { missing_tx_sets, apply_lag }`. The slot is
     restored only when ALL recorded causes have cleared:
     - **Missing tx_set** (#1795): cleared when the tx_set is cached.
       `Herder::cache_tx_set` calls
       `ScpDriver::resolve_missing_tx_set(&hash)` and then
       `SCP::restore_slot_fully_validated(slot)` for each newly
       all-clear slot.
     - **Apply-lag** (#1798 / H-014, #2096): cleared when the LCL
       advances past the slot's predecessor. `Herder::ledger_closed`
       calls `ScpDriver::resolve_apply_lag_for_next_index(slot + 1)`
       and then `SCP::restore_slot_fully_validated(slot)` for each
       newly all-clear slot.
     A slot may carry both causes simultaneously if an LCL advance
     races between the two recordings; restoration still requires ALL
     causes clear (the structure permits coexistence — see
     `DeferredCauses` in `crates/herder/src/scp_driver.rs`).
   - **Rationale**: The fast-path fixed a post-catchup stall where
     evicted tx_sets blocked EXTERNALIZE from reaching SCP, and the
     synchronous drain avoided an extra round-trip during catchup. The
     `MaybeValidDeferred` variant + paired restoration triggers preserve
     both behaviors while avoiding the secondary bugs where the local
     validator's own EXTERNALIZE emission would be permanently
     suppressed.
   - **No re-validation after tx_set arrival (#1796):**
     `process_ready_fetching_envelopes` only drains buffered
     non-EXTERNALIZE envelopes. The EXTERNALIZE that bypassed buffering
     via the fast-path is NOT re-validated when the tx_set arrives
     because `ValidationLevel` is ephemeral (not stored per-envelope in
     SCP). The end-state symmetry with `FullyValidated` is achieved by
     the herder-side restoration trigger flipping `fully_validated`
     back to `true` once the tx_set is cached (or apply catches up),
     producing identical slot end states: externalized,
     `fully_validated=true`, same emission visibility.
   - **Long-term direction**: The structural divergence is henyey's
     synchronous `drain_and_process_pending` on the externalize
     callback. Mirroring stellar-core's `safelyProcessSCPQueue →
     postOnMainThread` would close the apply-lag window entirely and
     obsolete `MaybeValidDeferred` and the herder-side restoration
     mechanism. Tracked as a follow-up.

8. **Error Handling**
   - **stellar-core**: C++ exceptions and integer result codes
   - **Rust**: `Result<T, HerderError>` with `thiserror` derive macros
   - **Rationale**: Idiomatic Rust error handling with no exceptions

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| HerderTests | 38 TEST_CASE / 277 SECTION | 26 `#[test]` + 54 scp_driver | Broad unit coverage; driver tests significantly expanded |
| PendingEnvelopesTests | 1 TEST_CASE / 20 SECTION | 24 `#[test]` | Good unit coverage; missing cost-tracking parity |
| QuorumIntersectionTests | 28 TEST_CASE / 0 SECTION | 12 `#[test]` (herder) + 7 integration (scp) | Core algorithm + interrupt integration tests |
| QuorumTrackerTests | 2 TEST_CASE / 10 SECTION | 10 unit tests | Good coverage |
| TransactionQueueTests | 18 TEST_CASE / 157 SECTION | 118 `#[test]` + 7 integration | Strong coverage; fee release and drop now tested |
| TxSetTests | 10 TEST_CASE / 66 SECTION | 30 tx_set + 27 tx_set_utils + 22 parallel | Strong coverage across modules |
| UpgradesTests | 29 TEST_CASE / 109 SECTION | 20 `#[test]` | Major gap; missing ledger-integrated config-upgrade tests |

### Test Gaps

- **HerderTests**: Missing integration tests for full envelope processing flow, ledger close lifecycle, out-of-sync recovery, quorum map reanalysis, and upgrade scheduling. SCP driver tests (54) cover significant validation and signing paths.
- **TransactionQueue**: Missing tests for arbitrage damping and filtered-account overrides. Fee release, drop, and rebroadcast testing improved. `broadcast_with_visitor()` now lives directly on `TransactionQueue` with priority ordering, ops budget, and budget-neutral skipped-tx semantics matching stellar-core.
- **TxSet**: Missing `ApplicableTxSetFrame` validation tests, phase ordering tests, and history-tx-set construction tests
- **Upgrades**: Missing ledger-integrated upgrade application tests, config upgrade set tests, and nomination-timeout stripping behavior
- **QuorumIntersection**: Core algorithm implemented (SCC + MinQuorumEnumerator). Missing some stellar-core-specific test scenarios (28 TEST_CASE). Herder integration with interrupt support is complete.

## Parity Notes

- **Local quorum set normalization in `Herder::build`** (issue #1953): `Herder::build` now normalizes `config.local_quorum_set` before distributing it to `SCP::new`, `ScpDriverConfig`, `FetchingEnvelopes`, `QuorumSetTracker`, and `SlotQuorumTracker`. This matches stellar-core's `LocalNode::LocalNode` which calls `normalizeQSet` on the local quorum set before computing `mQSetHash`. The app config path (`QuorumSetConfig::to_xdr` in `crates/app/src/config.rs`) already normalized, so this is defense-in-depth for direct API callers.
- **Remaining parity debt**: `set_local_quorum_set` does not normalize (matching upstream `LocalNode::updateQuorumSet`), but henyey only applies it to new slots while stellar-core's shared `LocalNode` reference affects all slots retroactively.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 138 |
| Gaps (None + Partial) | 37 |
| Intentional Omissions | 13 |
| **Parity** | **138 / (138 + 37) = 79%** |
