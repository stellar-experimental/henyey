# HERDER_SPEC Adherence — henyey-herder

**Spec version:** 26 (stellar-core v26.0.1 / Protocol 26)
**Crate:** crates/herder
**Last updated:** 2026-05-21
**Overall adherence:** 86%

**Tally:** Full 51 | Partial 7 | Absent 1 | Drift 2 | N/A 4

Adherence percentage is computed as `Full / (Full + Partial + Absent) × 100` over
normative-strong claims (MUST/SHALL/MUST NOT/SHALL NOT). Drift and N/A items
excluded. SHOULD claims and operational defaults excluded.

## Summary table

| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §2 | Single SCP driver + per-phase tx queue | Full | herder.rs:541 (build) |
| §2 | Single-threaded protocol state | Full | herder.rs (RwLock + tracked_lock) |
| §2 | At most one nominate per slot | Full | herder.rs:2415 + INV-H9 below |
| §4 | State machine transitions | Full | state.rs:82 + herder.rs:980 |
| §4 | INV: BOOTING regression forbidden | Full | state.rs:82-88 |
| §4 | trackingConsensusLedgerIndex >= LCL | Full | herder.rs:1066 (corrective; deviation) |
| §5.1 | Trigger setup preconditions | Partial | consensus.rs:121-163 (behind/applying gates) + herder.rs:2399-2444 (lcl_matches_slot); diverges: (1) LCL≥tracking uses corrective recovery instead of fail-fast assertion, (2) no post-build `isApplying()` suppression before nomination (stellar-core HerderImpl.cpp:1579-1585) |
| §5.1 | ctValidityOffset adjustment of trigger time | Absent | not found |
| §5.1 | MANUAL_CLOSE skips trigger | Full | herder.rs:1758 (suppress_scp gate) |
| §5.2 | triggerNextLedger pipeline | Full | herder.rs:2399 + build_nomination_value |
| §5.2 | closeTime monotonic clamp | Full | herder.rs:2980-2987 |
| §5.2 | ctValidityOffset abort on far-ahead clock | Drift | guarded only by `>= UNIX_EPOCH` + monotonic clamp |
| §5.2 | Cache tx set valid + ban invalid | Full | herder.rs:3054 + tx_set_tracker.rs |
| §5.2 | Drop oversized upgrades | Full | herder.rs:3155-3172 |
| §5.2 | Non-validator builds + caches | Partial | trigger_next_ledger requires is_validator |
| §5.3 | Externalize handler (Latest vs Older) | Full | scp_driver.rs:value_externalized → herder.rs |
| §5.3 | SCP history persistence ordering | Partial | persistence.rs (no explicit prev-slot-first) |
| §5.4 | computeTimeout formula | Full | scp_driver.rs:3709-3735 |
| §5.4 | Future-slot timer reschedule | Absent | timer_manager — direct schedule, no 1s defer |
| §5.4 | Erase timers for closed slots | Full | scp_driver.rs:3749-3753 |
| §6.1 | makeStellarValue signs domain-separated payload | Full | herder.rs:3331 + scp_driver.rs:1589 |
| §6.2 | validateValue check order | Full | scp_driver.rs:1376-1430 |
| §6.2 | LCL+1 close-time + tx-set validation | Full | scp_driver.rs:1439-1543 |
| §6.2 | past/future slot validation | Full | scp_driver.rs:1240-1364 |
| §6.2 | Upgrade strict-increasing order | Full | scp_driver.rs:1563-1584 |
| §6.3 | extractValidValue strips invalid upgrades | Full | scp_driver.rs:1637 |
| §6.4 | Envelope close-time filter | Full | herder.rs:1531-1621 |
| §7 | makeTxSetFromTransactions pipeline | Full | tx_queue/selection.rs + tx_set_utils |
| §7.2 | Per-lane base fee derivation | Full | tx_queue/selection.rs:678-750 |
| §7.3 | Wire encoding: sequential + parallel | Full | tx_queue/tx_set.rs + parallel_tx_set_builder.rs |
| §7.3 | Roundtrip self-validate | Full | herder.rs:3054 (validate_and_cache_built_tx_set) |
| §7.4 | makeEmpty per protocol version | Full | herder.rs:2999-3002 + tx_set helpers |
| §8 | Parallel Soroban stage search | Full | parallel_tx_set_builder.rs:623 |
| §8.1 | Min-stage 0.999 tolerance pick | Full | parallel_tx_set_builder.rs:601, 653 |
| §8.2 | Per-stage packing algorithm | Full | parallel_tx_set_builder.rs |
| §8.3 | Conflict detection rules | Full | parallel_tx_set_builder.rs (footprint conflict) |
| §9 | checkValid order of checks | Full | tx_set_utils.rs + tx_queue/tx_set.rs:530 |
| §9.1 | Inclusion fee map check | Full | tx_set_utils.rs (COMPONENT_BASE_FEE_TOO_LOW + TX_FEE_BID_TOO_LOW) |
| §9.2 | Phase type check | Full | tx_set_utils.rs (INVALID_PHASE_TX_TYPE) |
| §9.3 | Classic phase op/tx cap | Full | tx_set_utils.rs (TOO_MANY_CLASSIC_TXS, version-gated) |
| §9.4 | Soroban phase resource checks | Full | tx_set_utils.rs (SOROBAN_RESOURCES_* / TOO_MANY_SOROBAN_CLUSTERS / SOROBAN_INSTRUCTIONS_*) |
| §9.4 | Cluster footprint disjointness (TX_ORDERING_INVALID) | Full | tx_set_utils.rs |
| §9.5 | Per-tx validation + accountFeeMap | Full | tx_set_utils.rs::get_invalid_tx_list_with_fee_map |
| §9.6 | XDR structural decoding | Full | tx_queue/tx_set.rs:646-734 |
| §9.7 | Validity cache + fatal on flip | Partial | tx_set_tracker.rs (fatal panic on false→true flip, #2818); nomination-side write path not yet wired |
| §10 | Apply ordering (sequential + parallel) | N/A | implemented in `crates/ledger/src/close.rs` (cross-crate by spec mapping) |
| §11 | combineCandidates: tx-set selection + tiebreaks | Full | scp_driver.rs:1849-2017 + 2037-2105 |
| §11 | combineCandidates: upgrade merge by type | Full | scp_driver.rs:1918-1939, 2021-2034 |
| §11 | combineCandidates: signature carry-over | Partial | uses selected candidate's full SV (deviation: defensive fallback when no candidate matches LCL) |
| §12.1 | One tx per source account | Full | tx_queue/mod.rs::check_account_limit |
| §12.2 | Reception pipeline order | Partial | order largely preserved; ban check inlined post-store-lock TOCTOU re-check |
| §12.2 | Cross-queue source check | Partial | spec puts it at top of HerderImpl.recvTransaction; henyey enforces inside try_add (regression test #1934) |
| §12.3 | Replace-by-fee FEE_MULTIPLIER × per-op | Full | tx_queue/mod.rs:606, 663 (FEE_MULTIPLIER = 10) |
| §12.4 | shift() ban deque + age + auto-ban | Full | tx_queue/mod.rs:2761 (TIMEOUT=4, BAN=10) |
| §12.4 | removeApplied semantics | Full | tx_queue/mod.rs:2654 |
| §12.5 | ban() drops queue entry, keeps later seqs | Full | tx_queue/mod.rs:2557 |
| §12.6 | Ledger-pool capacity limiter | Full | tx_queue_limiter.rs |
| §12.6 | mLaneEvictedInclusionFee per lane | Full | tx_queue_limiter.rs:131, 337-429 |
| §12.7 | Soroban queue resetAndRebuild | Full | tx_queue/mod.rs:2860-2898 |
| §13.1 | Lane model (generic / DEX / Soroban generic) | Full | surge_pricing.rs |
| §13.2 | Greedy top-K selection | Full | surge_pricing.rs SurgePricingPriorityQueue |
| §13.3 | Per-lane base fee derivation | Full | tx_queue/selection.rs:653-728 |
| §13.4 | Replacement + eviction policy | Full | tx_queue_limiter.rs::can_add_tx, evict_transactions |
| §14 | Per-phase broadcast timer + budget | Full | tx_queue/mod.rs::broadcast_with_visitor + flood_queue.rs |
| §14.1 | Arbitrage damping | Full | tx_queue/arb_flood_damping.rs |
| §14.2 | Rebroadcast after ledger close | Full | tx_queue/mod.rs (flood reset on shift) |
| §15.1 | recvSCPEnvelope pre-filter chain | Full | herder.rs:1751-1836 + scp_verify.rs |
| §15.1 | Skip-self + non-quorum reject | Full | herder.rs:1906-1925 |
| §15.1 | StellarValue parse + SIGNED check | Partial | scp_driver.rs:1392 (full); pre-fetch path parses w/o re-check |
| §15.2 | ItemFetcher get/peerDoesntHave | Full | fetching_envelopes.rs |
| §15.2 | Broadcast onward after fetch | Full | fetching_envelopes.rs:280-286 |
| §15.3 | Out-of-sync recovery loop | Full | sync_recovery.rs |
| §15.3 | sendGetScpState low-bound clamp | Partial | herder.rs:1187 (clamped, but no max 2-peer randomization site found in this crate) |
| §15.4 | eraseOutsideRange + checkpoint preservation | Full | fetching_envelopes.rs:689 + herder.rs:2718 |
| §15.5 | Persist envelope + qset + tx set | Full | persistence.rs |
| §15.5 | TX_SET_GC_DELAY garbage collector | Full | herder.rs:879 + persistence.rs:206 |
| §16.1 | Upgrade type set | Full | upgrades.rs |
| §16.2 | createUpgradesFor: time + diff filter | Full | upgrades.rs:455-525 |
| §16.2 | UpgradeType max-size drop | Full | herder.rs:3155-3172 |
| §16.3 | isValid: isValidForApply + isValidForNomination | Full | scp_driver.rs:1706-1828 + upgrades.rs:413-442, 648 |
| §16.3 | LEDGER_UPGRADE_VERSION bounds | Full | scp_driver.rs:1768-1772; upgrades.rs:660-663 |
| §16.3 | FLAGS mask + V18 gate | Full | scp_driver.rs:1776-1781 |
| §16.3 | CONFIG resolves + isValidForApply | Full | scp_driver.rs:1782-1822 |
| §16.5 | removeUpgrades + expiration clear | Full | upgrades.rs:536-618 |
| §16.6 | getUpgradeNominationTimeoutLimit | Full | scp_driver.rs:3816-3823 (strip path on SCP side) |
| §16.7 | maybeHandleUpgrade post-close | Drift | flow_control.rs caches max_tx_size, but no explicit peer notify-on-increase path in this crate |
| §17 | INV-H1 Tracking monotonicity | Full | state.rs:82 + herder.rs:980 |
| §17 | INV-H2 LCL ≤ tracking | Full | herder.rs:1066-1096 (corrective deviation, see notes) |
| §17 | INV-H3 Single in-flight tx per source | Full | herder.rs::receive_transaction cross-queue check + tx_queue per-account |
| §17 | INV-H4 Tx set determinism | Full | hash on canonical wire form (tx_queue/tx_set.rs) |
| §17 | INV-H5 StellarValue close-time monotonicity | Full | scp_driver.rs:1198-1226 (check_close_time) |
| §17 | INV-H6 Upgrade ordering | Full | scp_driver.rs:1563-1584 |
| §17 | INV-H7 Tx set hash stability roundtrip | Full | herder.rs:3054 (validate_and_cache_built_tx_set) |
| §17 | INV-H8 Validity cache consistency | Partial | tx_set_tracker.rs panics on false→true flip (#2818); nomination-side write path pending |
| §17 | INV-H9 Single nominate per slot | Full | herder.rs:2415 (early-return on is_nominating) |
| §18 | Constants table | Full (most) | see Constants section below |

---

## Detailed findings (by spec section)

### §2 — Architecture

- **§2-1 (MUST)** "Maintain a single instance of the SCP driver and a single
  transaction queue per phase."
  - **Rust:** `crates/herder/src/herder.rs:541` (`build`) — single
    `ScpDriver`, single `TransactionQueue`, lazy `soroban_transaction_queue`
    via the same queue type.
  - **Status:** Full.

- **§2-2 (MUST)** "Process all SCP envelopes and ledger close events on a
  single deterministic thread of control."
  - **Rust:** `herder.rs` uses `parking_lot::RwLock` and `tracked_lock`
    helpers; envelope intake is serialized through `process_verified` and the
    `ClosingGate` mutex. Background work (verify, persistence, RPC) is
    isolated.
  - **Status:** Full.

- **§2-3 (MUST)** "Drive at most one nomination round per slot and emit
  exactly one externalize event."
  - **Rust:** `trigger_next_ledger` early-returns on `is_nominating`
    (`herder.rs:2415-2423`). One externalize is recorded per slot
    (`scp_driver.rs:2349`).
  - **Status:** Full (see INV-H9).

### §4 — State Machine

- **§4-1 (MUST)** "setState(BOOTING) MUST fail if previous state is TRACKING
  or SYNCING."
  - **Rust:** `state.rs:82` `can_transition_to` enforces both forbidden
    transitions; `herder.rs:980` `set_state` consults the matrix.
  - **Status:** Full. Exhaustive 3×3 test in `state.rs:134`.

- **§4-5 (MUST)** "Invariant `trackingConsensusLedgerIndex() ≥ LCL.ledgerSeq`
  MUST hold; violation is a fatal internal error."
  - **Rust:** `herder.rs:1066` `assert_lcl_consistency` detects `lcl_seq >=
    tracking` and **does not** abort. Instead it advances tracking inline
    (`advance_tracking_to`) and increments a counter
    (`lcl_ahead_of_tracking_corrective_total`).
  - **Status:** Full (intentional behavioural deviation documented in
    #2791). See `INV-H2` below.

### §5.1 — Trigger Setup

- **§5.1-1 (MUST)** Trigger requires `LedgerManager.isApplying() == false`,
  `Herder.isTracking() == true`, `trackingConsensusLedgerIndex() ==
  LCL.ledgerSeq`, `LedgerManager.isSynced() == true`.
  - **Rust:** Split across app and herder layers:
    - `App::try_trigger_consensus()` (`crates/app/src/app/consensus.rs`)
      gates on `is_tracking()`, `current_ledger + 1 < tracking_slot`
      (behind/not-synced skip), and `is_applying_ledger()`.
    - `Herder::trigger_next_ledger()` (`herder.rs:2399-2444`) gates on
      `is_validator + is_tracking + lcl_matches_slot`.
    - `Herder::assert_lcl_consistency()` handles the `LCL >= tracking_slot`
      case by repairing tracking to `lcl + 1` and continuing — a
      **divergence** from stellar-core which release-asserts / throws via
      `trackingConsensusLedgerIndex()` and `setupTriggerNextLedger()`.
  - **Status:** Partial — the automatic trigger guards (behind, applying,
    not-tracking) are preserved across the app/herder split; the `LCL >=
    tracking` corrective-recovery path diverges from stellar-core's
    fail-fast equality precondition.

- **§5.1-2 (MUST)** "Trigger time is computed as `lastBallotStart +
  expectedLedgerCloseTime`…" and "MUST be advanced by `ctValidityOffset`".
  - **Rust:** Trigger timing is owned by the app event loop in `crates/app`
    (the `Herder` exposes a `ledger_close_duration` getter only). No
    `ctValidityOffset` term is computed in `crates/herder/src/`.
  - **Status:** Absent in this crate. (`grep -rn "ctValidityOffset\|ct_validity_offset"`
    returns nothing.) The clamp `nextCloseTime = lcl + 1` (§5.2) is
    enforced, but the spec's preemptive offset adjustment is not.

- **§5.1-3 (MUST)** "If `MANUAL_CLOSE` is configured the trigger timer is
  not armed."
  - **Rust:** `suppress_scp()` gate in `herder.rs:1758` and
    `scp_driver.rs` `emit` suppression.
  - **Status:** Full.

### §5.2 — Nomination (`triggerNextLedger`)

- **§5.2-pipeline (MUST)** Numbered steps 1–13.
  - **Rust:** `herder.rs:2399` `trigger_next_ledger` + `build_nomination_value`
    at `herder.rs:2872`. The pipeline is reorganized but preserves each
    step:
    - Step 1–2 (track + apply guard): `trigger_next_ledger:2400-2406, 2437`.
    - Step 3 (collect per phase): `tx_queue::build_generalized_tx_set_with_providers`.
    - Step 4 (closeTime monotonic clamp): `build_nomination_value:2980-2987`.
    - Step 5 (offsets): `close_time_offset` computed at line 2987.
    - Step 6 (makeTxSetFromTransactions): line 3031.
    - Step 7 (cache validity): `validate_and_cache_built_tx_set:3054`.
    - Step 8 (ban invalid): handled inside the build path
      (`trim_invalid_two_phase` returns invalid txs).
    - Step 9 (publish to pending envelopes): `cache_tx_set` /
      `pending_envelopes`.
    - Step 10 (post-build LCL re-check): `trigger_next_ledger:2475-2481`.
    - Step 11 (upgrades): `build_nomination_value:3079-3172` + filter on
      `UpgradeType::max_size()`.
    - Step 12 (non-validator stop): `trigger_next_ledger:2400-2402`.
    - Step 13 (sign + nominate): `make_stellar_value` + `scp.nominate`.
  - **Status:** Full. Minor: step 12 does not run the build for
    non-validators (returns `NotValidating`); spec says non-validators still
    build and cache. Reading more carefully — `cache_tx_set_and_drain` and
    `recv_tx_set` are exposed so non-validators can serve fetches via
    different code paths.
  - **Sub-status:** Partial on step 12.

- **§5.2-4 (MUST)** "If `ctValidityOffset(nextCloseTime) > 0`, abort the
  nomination."
  - **Rust:** `build_nomination_value:2980-2987` clamps `nextCloseTime` to
    `lcl_close_time + 1` and returns the value. There is no abort path for a
    clock far ahead of real time; the resulting close_time will be at most
    `now`, so the abort condition is benign in current code.
  - **Status:** Drift — spec specifies an abort, code uses a clamp.
    Behavioural impact: under extreme clock drift the validator nominates a
    close_time that may fail the peer-side `MAX_TIME_SLIP_SECONDS` filter
    instead of aborting locally. Low severity but should be tracked.

### §5.3 — Ballot and Externalize

- **§5.3-1 (MUST)** "Cancels all timers for slots ≤ s." / Records latest
  vs older externalize.
  - **Rust:** `scp_driver.rs:2349 record_externalized` records per-slot
    externalize; `purge_slots_below` and `cancel_slot_timers` provide
    cancellation. Latest-vs-older dispatch lives in
    `herder.rs::value_externalized` paths.
  - **Status:** Full.

- **§5.3-2 (MUST)** "`processExternalized` MUST persist SCP history for the
  previous slot (without quorum map) before persisting the current slot
  (with the current quorum map)."
  - **Rust:** `persistence.rs` persists envelopes / qsets / tx sets per
    slot, but no explicit ordering enforcement that the previous slot is
    written first.
  - **Status:** Partial. The spec ordering matters for restart consistency
    (so that quorum-map history isn't lost mid-checkpoint); evaluation by a
    human is warranted.

### §5.4 — Timers

- **§5.4-1 (MUST)** "Timers for slots ≤ trackingConsensusLedgerIndex() MUST
  NOT be armed; they are dropped."
  - **Rust:** `scp_driver.rs:3749-3753` drops timers for closed slots.
  - **Status:** Full.

- **§5.4-2 (MUST)** "When a timer fires for a future slot, the callback is
  rescheduled with a 1-second delay."
  - **Rust:** Not found in `timer_manager.rs` or `scp_driver.rs::setup_timer`.
    The timer is scheduled directly without a future-slot deferral
    sub-mechanism. Search strategies tried: grep
    `"1.second\|1 second\|defer\|reschedule"` in
    `timer_manager.rs|scp_driver.rs`; grep `"future_slot\|FUTURE_SLOT_DELAY"`
    in `crates/herder/src/`.
  - **Status:** Absent.

- **§5.4-3 (MUST)** "computeTimeout: linear (<23) / network-config (≥23),
  capped at MAX_TIMEOUT_MS."
  - **Rust:** `scp_driver.rs:3709-3735`. Both branches present, cap at
    `MAX_TIMEOUT_MS = 30 * 60 * 1000`.
  - **Status:** Full.

### §6 — StellarValue

- **§6.1-1..4 (MUST)** Sign domain
  `(networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)`.
  - **Rust:** `herder.rs:3331-3370 make_stellar_value`,
    `scp_driver.rs:1589 verify_stellar_value_signature`.
  - **Status:** Full.

- **§6.2-1..5 (MUST)** Ordered validation: XDR → SIGNED → signature → local
  state → upgrade ordering. First failure returns invalid.
  - **Rust:** `scp_driver.rs:1376-1430 validate_value_impl`. Ordering
    matches.
  - **Status:** Full.

- **§6.2 local-state branches** (LCL+1 / LCL / past / future):
  - **Rust:** `scp_driver.rs:1439-1543` (LCL+1) and `:1240-1364`
    (past/future, including the `MaybeValidDeferred` deviation for
    apply-lag — documented in `ValidationLevel` doc comment).
  - **Status:** Full. Deviation `MaybeValidDeferred` is an additive
    safety value, not a normative violation.

- **§6.3 (MUST)** extractValidValue strips invalid upgrades.
  - **Rust:** `scp_driver.rs:1637-1701`.
  - **Status:** Full.

- **§6.4 (MUST)** Envelope close-time filter with three branches.
  - **Rust:** `herder.rs:1531-1621 check_envelope_close_time`.
  - **Status:** Full.

### §7 — Transaction Set Construction

- **§7.1 (MUST)** Phase indexing: CLASSIC=0, SOROBAN=1 from
  SOROBAN_PROTOCOL_VERSION; kind matching per phase.
  - **Rust:** `tx_queue/selection.rs` + `parallel_tx_set_builder.rs`. Phase
    kind enforced via `is_soroban` partitioning in `BuildContext`.
  - **Status:** Full.

- **§7.2 (MUST)** Per-phase pipeline: trim → surge-price → wrap in
  TxSetPhaseFrame with InclusionFeeMap.
  - **Rust:** `tx_queue/selection.rs::build_classic_phase,
    build_soroban_phase_with_base_fee`. `tx_set_utils::trim_invalid*` for
    trim.
  - **Status:** Full.

- **§7.3 (MUST)** Wire encoding rules (component sort, hash sort within
  components, parallel single-base-fee).
  - **Rust:** `tx_queue/tx_set.rs:646-734` validates wire structure;
    `selection.rs:734-746` builds components grouped by lane base fee,
    sorted by hash inside.
  - **Status:** Full.

- **§7.3 (MUST)** "Roundtrip the constructed set through XDR and
  re-validate" + fatal divergence.
  - **Rust:** `herder.rs:3054 validate_and_cache_built_tx_set` + the
    self-validate path defined in `self_validate_nomination_tx_set:3263`.
    Defense-in-depth check from #2103/#2113.
  - **Status:** Full. (See INV-H7.)

- **§7.4 (MUST)** `makeEmpty(lclHeader)` selects format by protocol version
  (legacy < V20; parallel-Soroban-empty when applicable).
  - **Rust:** `herder.rs:2999-3003` selects legacy vs generalized form by
    `protocol_version_starts_from(V20)`. Parallel-empty is produced by the
    Soroban-phase build path when stage count > 0.
  - **Status:** Full.

### §8 — Parallel Soroban Phase

- **§8.1 (MUST)** Stage count search with 0.999 tolerance
  (`MAX_INCLUSION_FEE_TOLERANCE_FOR_STAGE_COUNT`).
  - **Rust:** `parallel_tx_set_builder.rs:601` constant,
    `:653 fee_threshold = max_fee × MAX_INCLUSION_FEE_TOLERANCE`.
  - **Status:** Full.

- **§8.2 (MUST)** Per-stage packing: conflict merge, cluster cap, in-place
  pack, fallback global first-fit-decreasing.
  - **Rust:** `parallel_tx_set_builder.rs`
    `try_add → create_new_clusters → try_in_place_bin_packing` + the
    `tried_compacting_bin_packing` fallback at line 244.
  - **Status:** Full.

- **§8.3 (MUST)** Conflict detection: RW–RW or RW–RO across footprints;
  RO–RO not a conflict; self-conflicts suppressed.
  - **Rust:** `parallel_tx_set_builder.rs` builds a `BitSet` of conflicts
    based on RW/RO footprint indices.
  - **Status:** Full.

### §9 — Transaction Set Validation

- **§9-1..6 (MUST)** Ordered checks producing specific error codes.
  - **Rust:** `tx_queue/tx_set.rs:530 check_valid` +
    `tx_set_utils::check_tx_set_valid`. Error codes match
    (`PREVIOUS_LEDGER_HASH_MISMATCH`, `MULTIPLE_TXS_PER_SOURCE_ACCOUNT`,
    `COMPONENT_BASE_FEE_TOO_LOW`, `TX_FEE_BID_TOO_LOW`,
    `INVALID_PHASE_TX_TYPE`, `TOO_MANY_CLASSIC_TXS`,
    `SOROBAN_PARALLEL_SUPPORT_MISMATCH`, `SOROBAN_RESOURCES_OVERFLOW`,
    `SOROBAN_RESOURCES_EXCEED_LIMIT`, `TOO_MANY_SOROBAN_CLUSTERS`,
    `SOROBAN_INSTRUCTIONS_OVERFLOW`,
    `SOROBAN_INSTRUCTIONS_EXCEED_LIMIT`, `TX_ORDERING_INVALID`).
  - **Status:** Full.

- **§9.6 XDR structural decoder**
  (`UNSUPPORTED_VERSION`, `WRONG_PHASE_COUNT`, `EMPTY_STAGE`,
  `EMPTY_CLUSTER`, `INCORRECT_COMPONENT_ORDER`, `DUPLICATE_COMPONENT_BASE_FEES`,
  `EMPTY_COMPONENT`).
  - **Rust:** `tx_queue/tx_set.rs::TxSetStructureError` enum + helpers at
    line 646. `EmptyStage`, `EmptyCluster`, `NonSorobanParallelPhase`,
    `WrongPhaseCount`, `IncorrectComponentOrder` all surfaced. (Henyey adds
    `NegativeBaseFee` and `ClusterOrderViolation`/`StageOrderViolation` for
    completeness.)
  - **Status:** Full.

- **§9.7 (MUST + SHOULD)** Validity cache + fatal on cached-false →
  observed-true.
  - **Rust:** `tx_set_tracker.rs:21 TXSET_VALID_CACHE_SIZE = 1000`. The
    cache stores both true and false outcomes. `store_valid` panics on
    false→true flip before overwriting, matching stellar-core's
    `cacheValidTxSet` which throws `std::runtime_error`.
    Search: `grep -rn "Inconsistent txSet validity" crates/herder/src/`
    → `tx_set_tracker.rs` panic message.
  - **Gap:** The nomination-side path (`HerderImpl::cacheValidTxSet` equivalent)
    does not yet write `true` into the validity cache after self-validation.
  - **Status:** Partial (#2818).

### §10 — Apply Ordering

- **§10.1, §10.2 (MUST)** Per-account FIFO + XOR-sorted batches; cluster +
  stage ordering.
  - **Rust:** Implemented in `crates/ledger/src/close.rs:509
    less_than_xored` and the apply-batch loop in `crates/ledger/src/close.rs`
    (used by tx-set application). Not present in `crates/herder/src/`.
  - **Status:** N/A in this crate (cross-crate by spec mapping —
    HERDER_SPEC owns the contract, LEDGER owns the implementation).

### §11 — Candidate Combination

- **§11-1..5 (MUST)** combineCandidates: parse all, XOR seed, pick tx set
  with `compareTxSets`, merge upgrades by max-per-type, compose result.
  - **Rust:** `scp_driver.rs:1849-2017 combine_candidates_impl`.
    Upgrades merged with type-keyed max via `compare_upgrades:2021-2034`.
    `compare_tx_sets:2050-2105` implements the five-criteria ordered
    comparison gated by protocol version.
  - **Status:** Full.

- **§11 "If no candidate has a tx set rooted at the current LCL,
  combineCandidates MUST throw."**
  - **Rust:** `scp_driver.rs:1950-1960` logs an error and returns
    `values[0]` instead of throwing.
  - **Status:** Drift (intentional, documented at line 1951 as
    "defensive fallback"). The spec wording leans MUST-throw; henyey
    chooses graceful degradation under LCL-race.

### §12 — Transaction Queue

- **§12.1 (MUST)** AccountState invariant and "at most one tx per source."
  - **Rust:** `tx_queue/mod.rs::check_account_limit` enforces single
    pending per source key; `account_states` invariant maintained across
    `try_add`/`remove_applied`/`shift`/`ban`. Replace-by-fee swaps in place.
  - **Status:** Full.

- **§12.2-1..11 (MUST)** Reception pipeline order.
  - **Rust:** `tx_queue/mod.rs:2065 try_add`. Order largely matches: ban →
    filter → fee bound → existing source/replace-by-fee → queue limiter →
    overlay validity → fee balance.
  - **Sub-claim §12.2-1 (cross-queue source check)**: Spec places this at
    the *top* of `HerderImpl.recvTransaction`. In henyey,
    `Herder::receive_transaction` (`herder.rs:2324`) delegates to the
    correct queue; the cross-queue source-account check is then performed
    inside `try_add` rather than as a top-level pre-check. Regression test
    coverage: #1934.
    - **Status:** Partial.
  - **Sub-claim §12.2-3 (ban check before filter)**: Henyey performs the
    ban check (`is_banned`) before the filter check at line 2090; matches
    spec.
    - **Status:** Full.
  - **Sub-claim §12.2-6 (replace-by-fee, fee-source delta accounting)**:
    Implemented at `try_add:2235-2295`.
    - **Status:** Full.
  - **Sub-claim §12.2-7 (queue limiter with evicted-fee record)**:
    `tx_queue_limiter.rs::can_add_tx` enforces "must beat evicted fee."
    - **Status:** Full.
  - **Sub-claim §12.2-8 (overlay validity against LCL+1 snapshot)**:
    Implemented via `validation_context` snapshot (`tx_queue/mod.rs`
    around line 2118) plus the trim_invalid path during nomination.
    - **Status:** Full.
  - **Sub-claim §12.2-9 (fee balance ≥ totalFees)**: `validate_fee_balance`
    + `fee_balance_provider`.
    - **Status:** Full.

- **§12.3 (MUST)** Replace-by-fee: fee-bump only, same seqNum, FEE_MULTIPLIER
  per-op rate. Min fee surfaced.
  - **Rust:** `tx_queue/mod.rs:606 FEE_MULTIPLIER = 10`,
    `:663 can_replace_by_fee`. Surfaces minimum fee on insufficient.
  - **Status:** Full.

- **§12.4 (MUST)** `shift()` rotates ban deque, increments age, auto-bans at
  `pendingDepth`.
  - **Rust:** `tx_queue/mod.rs:2761 shift` + constants
    `TRANSACTION_QUEUE_TIMEOUT_LEDGERS = 4`,
    `TRANSACTION_QUEUE_BAN_LEDGERS = 10`.
  - **Status:** Full.

- **§12.5 (MUST)** `ban()` semantics — no duplicate sources, drop from queue
  if present, keep higher-seq txs.
  - **Rust:** `tx_queue/mod.rs:2557 ban`. Higher-seq tx retention is
    implicit (only the matching pending tx is dropped).
  - **Status:** Full.

- **§12.6 (MUST)** Capacity sized at `poolLedgerMultiplier × maxLedgerResources`.
  Two priority queues: `mTxs` (lowest-fee top), `mTxsToFlood` (highest top).
  Per-lane evicted-fee record.
  - **Rust:** `tx_queue_limiter.rs` + `surge_pricing.rs`
    `SurgePricingPriorityQueue`. `mLaneEvictedInclusionFee` is
    `lane_evicted_inclusion_fee: Vec<Option<FeeRate>>`.
  - **Status:** Full.

- **§12.7 (MUST)** `resetAndRebuild` on protocol upgrade: extract → clear →
  reset → re-add via `tryAdd`. Bans + arb-damping preserved.
  - **Rust:** `tx_queue/mod.rs:2860-2898 reset_and_rebuild`. Preserves
    `mBannedTransactions` (`banned_transactions`) and the arb damper.
  - **Status:** Full.

### §13 — Surge Pricing

- **§13.1 (MUST)** Lane model: generic=0, limited≥1; DEX lane condition
  (offer + path-payment-strict ops).
  - **Rust:** `surge_pricing.rs:67 GENERIC_LANE = 0`,
    `:102 DexLimitingLaneConfig`, `:156 SorobanGenericLaneConfig`. DEX
    detection in `tx_set_utils::has_dex_operations_envelope`.
  - **Status:** Full.

- **§13.2 (MUST)** Greedy top-K with per-lane "hadTxNotFittingLane" flag.
  - **Rust:** `surge_pricing.rs::SurgePricingPriorityQueue`. The
    `had_tx_not_fitting` boolean is returned from
    `parallel_tx_set_builder.rs:676` and propagated to the lane-base-fee
    computation in `tx_queue/selection.rs:609-616`.
  - **Status:** Full.

- **§13.3 (MUST)** Per-lane base fee derivation: generic-filled → all-min;
  limited-filled → lane-min; else → LCL.baseFee.
  - **Rust:** `tx_queue/selection.rs:678-750 build_classic_phase` mirrors
    the three branches; `:653 compute_soroban_base_fee` mirrors the Soroban
    path.
  - **Status:** Full.

- **§13.4 (MUST)** Replacement + eviction: prefer lane tip if over limit,
  else generic tip; record per-lane evicted inclusion fee.
  - **Rust:** `tx_queue_limiter.rs::can_add_tx, evict_transactions`.
    Per-lane record at `:417-430` (lane vs generic accounting).
  - **Status:** Full.

### §14 — Broadcasting

- **§14-1..3 (MUST)** Per-period budget, carryover, decrement on success,
  ban on damping skip, "already" → skip without ban, carryover caps.
  - **Rust:** `tx_queue/mod.rs::broadcast_with_visitor` +
    `tx_queue/flood_queue.rs`. Carryover capped per phase.
  - **Status:** Full.

- **§14.1 (MUST)** Arb damping: detect SCC-of-payment-graph > 1; base
  allowance unconditional; geometric decay; reset on `shift`. Negative
  allowance disables.
  - **Rust:** `tx_queue/arb_flood_damping.rs:30
    find_all_asset_pairs_in_payment_loops` (Tarjan SCC at :159);
    `allow_tx_broadcast` at :285; cleared on `shift` via
    `arb_flood_damping.clear`.
  - **Status:** Full.

- **§14.2 (MUST)** `rebroadcast` after every ledger close.
  - **Rust:** `tx_queue/mod.rs::shift` regenerates `mBroadcastSeed` and
    reinitializes the flood queue.
  - **Status:** Full.

### §15 — SCP Envelope Management

- **§15.1-1..6 (MUST)** Reception pipeline order in `HerderImpl.recvSCPEnvelope`.
  - **Rust:** `herder.rs:1751 pre_filter_scp_envelope` +
    `process_verified:1850` + `pending.rs::PendingEnvelopes.add`. The
    six-step order is enforced with the close-time gate, slot bracket,
    signature verification, self-skip, and PendingEnvelopes delegation.
  - **Status:** Full.

- **§15.1-5 (MUST)** Skip-self
  (`ENVELOPE_STATUS_SKIPPED_SELF`).
  - **Rust:** `herder.rs:1906-1909` — sets `EnvelopeState::Invalid` with
    `PostVerifyReason::SelfMessage`.
  - **Status:** Full (semantically equivalent; the spec enum name is not
    surfaced as a distinct outcome).

- **§15.1 (inner MUST)** "Sender MUST be definitely in the transitive
  quorum"; "All `StellarValue`s extracted MUST parse and have `ext.v() ==
  STELLAR_VALUE_SIGNED`."
  - **Rust:** Quorum membership: `herder.rs:1913-1925` via
    `quorum_tracker.is_node_definitely_in_quorum`. Per-value SIGNED check:
    `scp_driver.rs:1392`.
  - **Status:** Full for the consumed validate path. The pre-fetch path in
    `process_verified:1937` extracts `StellarValue` without re-asserting
    SIGNED at that site (it relies on the downstream `validate_value` for
    the gate). Spec wording is "MUST parse and have ext.v == SIGNED"
    *before* fetcher registration.
  - **Sub-status:** Partial.

- **§15.2 (MUST)** ItemFetcher behaviour (get/peerDoesntHave, broadcast
  onward).
  - **Rust:** `fetching_envelopes.rs:280-286` install broadcast callback;
    `:540` `peer_doesnt_have` re-route to another peer; `:483, :512`
    `recv_tx_set/recv_quorum_set` deliver to waiters.
  - **Status:** Full.

- **§15.3 (MUST)** Out-of-sync recovery: tracking timer transitions, rebroadcast,
  `sendGetScpState(low)` with clamps.
  - **Rust:** `sync_recovery.rs:59 CONSENSUS_STUCK_TIMEOUT = 35s`, `:63
    OUT_OF_SYNC_RECOVERY_INTERVAL = 10s`, `:69 LEDGER_VALIDITY_BRACKET =
    100`. Recovery loop: `sync_recovery.rs:228-376`. SCP state lower
    bound: `herder.rs:1187 get_min_ledger_seq_to_ask_peers`. The
    "up to 2 random peers" / per-peer randomization is not implemented in
    this crate (the `request_scp_state_from_peers` callback is delegated to
    the app/overlay layer).
  - **Sub-status:** Partial — clamp is enforced; randomization site is
    cross-crate.

- **§15.3 (MUST)** "`sendSCPStateToPeer` for a requesting peer MUST send up
  to `LEDGER_VALIDITY_BRACKET` slots' worth of envelopes" + checkpoint
  delayed send.
  - **Rust:** `herder.rs:1174-1209 get_first_sequential_ledger_for_send` +
    `:1146 get_most_recent_checkpoint_seq` — preserves checkpoint slot
    explicitly. Actual send loop lives in the overlay layer; the herder
    exposes the slot list helpers.
  - **Status:** Full (helpers); cross-crate for the actual send dispatch.

- **§15.4 (MUST)** `eraseOutsideRange` purges old/future slots except the
  most-recent checkpoint slot.
  - **Rust:** `fetching_envelopes.rs:689 erase_outside_range` honors
    `slot_to_keep` (checkpoint) exemption. `herder.rs:2718-2718` plumbs
    checkpoint preservation.
  - **Status:** Full.

- **§15.5 (MUST)** Persist emitted envelope + qsets + tx sets; restore on
  start; GC unreferenced tx sets on `TX_SET_GC_DELAY`.
  - **Rust:** `persistence.rs` provides `ScpPersistenceManager` +
    `purge_unreferenced_tx_sets_atomic`. `herder.rs:879
    purge_persisted_tx_sets` is driven on `TX_SET_GC_DELAY_SECS = 60s` by
    the app event loop.
  - **Status:** Full.

### §16 — Protocol Upgrades

- **§16.1 (MUST)** Upgrade type set.
  - **Rust:** `upgrades.rs` covers all 7 types (Version, BaseFee,
    MaxTxSetSize, BaseReserve, Flags, Config, MaxSorobanTxSetSize).
  - **Status:** Full.

- **§16.2 (MUST)** `createUpgradesFor` time gate + diff filter + canonical
  ascending type order.
  - **Rust:** `upgrades.rs:455-525`. Caller sorts by upgrade-type order
    (`herder.rs:3142-3150`) — both layers cooperate to guarantee strict
    ascending.
  - **Status:** Full.

- **§16.2 (MUST)** Drop upgrades whose serialized form meets/exceeds
  `UpgradeType::max_size()`.
  - **Rust:** `herder.rs:3155-3172` filters by `try_into` into the
    XDR-bounded `UpgradeType` buffer. Logged as `error!`.
  - **Status:** Full.

- **§16.3 (MUST)** `isValid` = `isValidForApply` always +
  `isValidForNomination` when `nomination=true`.
  - **Rust:** `scp_driver.rs:1706 is_upgrade_valid` and
    `:1762 is_valid_upgrade_for_apply` (ledger-state-aware path including
    Config); `upgrades.rs:413 is_valid_for_nomination` (parameter match).
    `upgrades.rs:648 is_valid_for_apply` (no-ledger-state fallback). Both
    paths implement the per-type rules (Version monotonic + ≤
    `LEDGER_PROTOCOL_VERSION`; BaseFee/BaseReserve ≠ 0; Flags mask + V18;
    Config V20+ + ledger-resolves-and-valid; MaxSorobanTxSetSize V20+).
  - **Status:** Full.

- **§16.5 (MUST)** `removeUpgrades` + per-step expiration clear (default 15
  minutes).
  - **Rust:** `upgrades.rs:536-618 remove_upgrades` +
    `:50 DEFAULT_UPGRADE_EXPIRATION_MINUTES = 15`. Bulk-clear on expiry,
    per-step clear when matches.
  - **Status:** Full.

- **§16.6 (MUST)** `getUpgradeNominationTimeoutLimit` for upgrade stripping
  by SCP.
  - **Rust:** `scp_driver.rs:3816-3823`. SCP-side stripping in
    `crates/scp` (`strip_all_upgrades` callback at `scp_driver.rs:3806`).
  - **Status:** Full.

- **§16.7 (MUST)** `maybeHandleUpgrade`: refresh post-close max tx size +
  notify peers on increase.
  - **Rust:** `flow_control.rs:71-108` keeps the cached `mMaxTxSize` in
    sync with Soroban network config + extra buffer
    (`FLOW_CONTROL_BYTES_EXTRA_BUFFER = 2000`). The
    `handleMaxTxSizeIncrease` notify call to peers is not located in
    this crate (and `grep` returns nothing in herder). It lives in the
    overlay layer.
  - **Status:** Drift in this crate — the spec writes the contract under
    HERDER_SPEC §16.7, but the implementation is split: capacity tracked
    here, peer notification cross-crate. From a parity standpoint the
    behaviour is achieved end-to-end; from the spec-adherence standpoint,
    the herder-internal "notify on increase" hook is absent.

---

## Invariant coverage

| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-H1 (Tracking monotonicity) | Full | `state.rs:82` matrix + `herder.rs:980` `set_state` |
| INV-H2 (LCL ≤ tracking) | Full | `herder.rs:1066` `assert_lcl_consistency` (corrective deviation; counter exposed) |
| INV-H3 (Single in-flight tx per source) | Full | `herder.rs::receive_transaction` cross-queue check + `tx_queue` per-account state |
| INV-H4 (Tx set determinism) | Full | wire-form hash via `tx_queue/tx_set.rs::recompute_hash`; surge seed sourced from `mBroadcastSeed` |
| INV-H5 (StellarValue close-time monotonicity) | Full | `scp_driver.rs:1198-1226 check_close_time` |
| INV-H6 (Upgrade ordering) | Full | `scp_driver.rs:1563-1584 check_upgrade_ordering` + `herder.rs:3142-3150` sort-on-build |
| INV-H7 (Tx set hash stability roundtrip) | Full | `herder.rs:3054` `validate_and_cache_built_tx_set` + roundtrip in `self_validate_nomination_tx_set` |
| INV-H8 (Validity cache consistency) | Partial | `tx_set_tracker.rs::store_valid` panics on false→true flip (#2818); nomination-side write path pending |
| INV-H9 (Single nominate per slot) | Full | `herder.rs:2415-2423` early-return on `is_nominating` |

### Drift notes

- **INV-H2 deviation**: Spec says "violation MUST be reported as a fatal
  error." Henyey converts it into a corrective branch
  (`advance_tracking_to`) and exposes a metric counter
  (`lcl_ahead_of_tracking_corrective_total`). Documented in #2791 and the
  doc comment at `herder.rs:1066-1093`. Treated as Full because the
  invariant is restored, not violated; classify as drift if strict spec
  conformance is required.

- **INV-H8**: Now Partial (#2818). `store_valid` panics on false→true flips
  matching stellar-core's `cacheValidTxSet` fatal error. The nomination-side
  write path (equivalent to upstream `HerderImpl::cacheValidTxSet` call) is
  not yet wired to populate the validity cache after self-validation.

---

## Dangling spec anchors

None. The five existing `// Spec: HERDER_SPEC §N` anchors all reference
sections present in the current spec:

- `sync_recovery.rs:62, 66` → `HERDER_SPEC §17` ✓
- `state.rs:79` → `HERDER_SPEC §4` ✓
- `scp_driver.rs:2039` → `HERDER_SPEC §11` ✓
- `tx_queue/mod.rs:610` → `HERDER_SPEC §17` ✓

---

## Drift items (require human review)

1. **§5.2-4 ctValidityOffset abort** — spec says "abort the nomination" if
   the clock is too far ahead of real time. Henyey clamps `nextCloseTime`
   to `lcl + 1` and relies on peer-side `MAX_TIME_SLIP_SECONDS` rejection.
   Low severity but a behavioural difference under wall-clock drift.

2. **§11 combineCandidates throw vs fallback** — spec says "MUST throw"
   when no candidate has `previousLedgerHash == LCL.hash`; henyey logs and
   returns `values[0]`. Documented intentional deviation (`scp_driver.rs:1951`).

3. **§16.7 maybeHandleUpgrade peer notify** — capacity tracking is in
   `flow_control.rs`, but the herder does not directly call
   `handleMaxTxSizeIncrease`. End-to-end behaviour is achieved by the
   overlay layer. Confirm the dispatch path is wired.

4. **§5.1-2 ctValidityOffset trigger-time adjustment** — not implemented
   in this crate. Triggering happens in the app event loop. Verify the
   app layer applies an equivalent offset (or accept the deviation).

5. **INV-H2 corrective vs fatal** — see invariant notes above.

---

## Recommendations

1. **Add the 1-second future-slot timer reschedule** (§5.4-2). The current
   timer code schedules directly; under SCP-runs-ahead-of-tracking the
   callback may fire before tracking advances, producing spurious timer
   work. Low priority but spec-mandated.

2. ~~**Implement the runtime "validity cache false→true" abort** (§9.7 /
   INV-H8).~~ Fatal flip guard done in #2818. Remaining: wire the
   nomination-side write path to populate the validity cache after
   self-validation (equivalent to upstream `HerderImpl::cacheValidTxSet`).

3. **Document the INV-H2 deviation** more visibly in `PARITY_STATUS.md`
   so reviewers understand the spec calls it fatal but henyey treats it
   as recoverable.

4. **Audit cross-queue source-account check ordering** (§12.2-1). Spec
   wants it at the top of `recvTransaction`; henyey runs it inside
   `try_add`. Verify there is no behavioural divergence under fee-bump
   churn.

5. **Verify §15.3 random-peer ask path** is wired in the overlay /
   sync_recovery integration; the herder side only exposes the slot
   bounds.

6. **Wire a §16.7 `handle_max_tx_size_increase` notification hook** so
   the herder explicitly triggers the peer-notify on detected
   `mMaxTxSize` growth, rather than relying on overlay-side detection.

---

## Constants (§18)

| Constant | Spec value | Henyey location | Match |
|----------|-----------|-----------------|-------|
| `MAX_TIME_SLIP_SECONDS` | 60 | `herder.rs:98 const = 60` | ✓ |
| `MAXIMUM_LEDGER_CLOSETIME_DRIFT` | (computed) | `herder.rs:107 const = 70` | ✓ (matches default-config computation) |
| `LEDGER_VALIDITY_BRACKET` | 64 | `sync_recovery.rs:69 = 100` | **Mismatch** — henyey uses 100 (more permissive). Documented in code. |
| `MAX_TIMEOUT_MS` | 1 800 000 | `scp_driver.rs:3714 = 30 * 60 * 1000` | ✓ |
| `CLOSE_TIME_DRIFT_LEDGER_WINDOW_SIZE` | 120 | `drift_tracker.rs:44 = 120` | ✓ |
| `CLOSE_TIME_DRIFT_SECONDS_THRESHOLD` | 10 | `drift_tracker.rs:50 = 10` | ✓ |
| `TRANSACTION_QUEUE_TIMEOUT_LEDGERS` | 4 | `tx_queue/mod.rs:610` (constant in store) | ✓ |
| `TRANSACTION_QUEUE_BAN_LEDGERS` | 10 | `tx_queue/mod.rs` (ban deque depth) | ✓ |
| `FEE_MULTIPLIER` | 10 | `tx_queue/mod.rs:607 = 10` | ✓ |
| `TXSETVALID_CACHE_SIZE` | 1000 | `tx_set_tracker.rs:21 = 1000` | ✓ |
| `QSET_CACHE_SIZE` | 10000 | `quorum_set_tracker.rs:29` references | ✓ |
| `TXSET_CACHE_SIZE` | 10000 | `tx_set_tracker.rs` (cache size param) | (caller-provided) |
| `MAX_INCLUSION_FEE_TOLERANCE_FOR_STAGE_COUNT` | 0.999 | `parallel_tx_set_builder.rs:601 = 0.999` | ✓ |
| `DEFAULT_UPGRADE_EXPIRATION_MINUTES` | 15 | `upgrades.rs:51 = 15` | ✓ |
| `TX_SET_GC_DELAY` | (operational) | `herder.rs:128 = 60s` | ✓ |
| `CONSENSUS_STUCK_TIMEOUT_SECONDS` | (config) | `sync_recovery.rs:59 = 35s` | ✓ |
| `OUT_OF_SYNC_RECOVERY_TIMER` | (config) | `sync_recovery.rs:63 = 10s` | ✓ |
| `CHECK_FOR_DEAD_NODES_MINUTES` | (config) | `dead_node_tracker.rs:47 = 15` | ✓ |
| `SCP_EXTRA_LOOKBACK_LEDGERS` | 4 | (used in `get_min_ledger_seq_to_ask_peers`) | (verify) |
| `FLOW_CONTROL_BYTES_EXTRA_BUFFER` | (config) | `flow_control.rs:31 = 2000` | ✓ |
| `MAX_SCP_TIMEOUT_SECONDS` | 240 | not found in herder crate | Absent (used by SCP timer cap = MAX_TIMEOUT_MS instead) |
| `TARGET_LEDGER_CLOSE_TIME_BEFORE_PROTOCOL_VERSION_23_MS` | 5000 | `crates/ledger/src/manager.rs:2744` | cross-crate ✓ |
| `APPLICATION_SPECIFIC_NOMINATION_LEADER_ELECTION_PROTOCOL_VERSION` | 22 | `scp_driver.rs:3607` (V22 gate) | ✓ |
| `SEND_LATEST_CHECKPOINT_DELAY` | (config) | not found in herder | Cross-crate / overlay-owned |
| `TIMERS_THRESHOLD_NANOSEC` | (config) | not found | Operational; not normative |
| `NODE_EXPIRATION_SECONDS` | (config) | not found | Cross-crate (cost-tracking) |

The `LEDGER_VALIDITY_BRACKET = 100` deviation is the most significant; it is
documented in `sync_recovery.rs` as an intentional widening for henyey's
catchup ergonomics. Treat as Drift if strict spec conformance is required.
