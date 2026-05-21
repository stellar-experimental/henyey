# LEDGER_SPEC Adherence — henyey-ledger

**Spec version:** 26 (stellar-core v26.0.1 / Protocol 26)
**Crate:** crates/ledger (with cross-references to crates/app)
**Last updated:** 2026-05-21
**Overall adherence:** 82%

**Counts:** Full 58 | Partial 8 | Absent 5 | Drift 3 | N/A 11

> **Architectural note.** `crates/ledger` deliberately replaces stellar-core's
> nested `LedgerTxn` chain with a flat `CloseLedgerState` wrapper composed of
> an immutable snapshot plus a single `LedgerDelta` change-map. Per
> `crates/ledger/PARITY_STATUS.md:35` this divergence is intentional and the
> behavioral surface (apply-then-commit, change merging, header re-stamping)
> is preserved without the parent/child seal/unseal mechanics. All claims
> below that depend specifically on `LedgerTxn` nesting, the `INIT/LIVE/
> DELETED` entry-state model, the `mActive` handle ledger, or the entry
> merge matrix are classified **N/A** with that rationale.

---

## Summary Table

| Section | Topic | Status | Implementation |
|---|---|---|---|
| §4.1 / §9.1 | Entry, seq increment, prev-hash, validation gate | Full | manager.rs:2073 `begin_close` |
| §4.2 step 9 | `txSet.previousLedgerHash() == prevHash` | Full | manager.rs:2114, app/ledger_close.rs:1897 |
| §4.2 step 10 | `txSet.getContentsHash() == ledgerData.value.txSetHash` | Full | app/ledger_close.rs:1962 |
| §4.2 step 12 | `prepareForApply` | Partial | app/ledger_close.rs:1982 (called pre-close, not inside `begin_close`) |
| §4.3 | LedgerCloseMeta construction | Full | manager.rs:5832 `build_ledger_close_meta` |
| §4.4 | Fee phase (`processFeesSeqNums`) | Full | manager.rs:4044 `pre_deduct_all_fees_on_delta`; `MAX_SEQ_NUM_TO_APPLY` markers installed via `execution/tx_set.rs:165,347-371` `compute_max_seq_num_to_apply` |
| §4.5 | Apply phase (sequential / parallel split) | Full | manager.rs:4044-4177 |
| §4.6 | `txSetResultHash = sha256(txResultSet)` | Full | manager.rs:4768 |
| §4.7 | `APPLYING → COMMITTING` transition | N/A | No `ApplyState` phase machine in Rust |
| §4.8 | Per-upgrade validate → apply → meta | Full | manager.rs:4369 `apply_upgrades_to_delta`, config_upgrade.rs:281 |
| §4.9 | Seal + persist | Partial | manager.rs:4757 `commit` — single delta drain, no parent/child seal |
| §4.10 step 25 | `expectedHash` check | Full | manager.rs:5536 |
| §4.11 | 8-step subtle sequence | Partial | Most steps inlined; LCL update before meta (Drift) |
| §5 | Apply state phase machine | N/A | No `SETTING_UP_STATE / READY_TO_APPLY / APPLYING / COMMITTING` states |
| §6.1 | Apply order, per-source seq-num ordering | Full | close.rs:603 `sorted_for_apply_sequential` |
| §6.2 step 4 (P19+) | `accToMaxSeq` + `mergeSeen` | Full | `execution/tx_set.rs:347-371` `compute_max_seq_num_to_apply` scans txs for AccountMerge, builds per-source max-seq map, installs via `set_max_seq_num_to_apply`; regression-tested by `test_audit_577_max_seq_num_to_apply_with_pre_charged` |
| §6.4 step 3 | `subSeed = SHA-256(base \|\| index)` | Full | execution/signatures.rs:615 `sub_sha256` |
| §6.5 | Parallel Soroban phase (stages/clusters) | Full | execution/tx_set.rs:644 `execute_soroban_parallel_phase` |
| §6.6 | `processPostTxSetApply` (Soroban refunds) | Full | execution/tx_set.rs:242,902 |
| §7.1 | Nested LedgerTxn / single-child / same-thread | N/A | Flat `CloseLedgerState`; no parent/child relation |
| §7.4 | Seal semantics (sealed-after-commit) | N/A | No seal state; close finishes by draining delta |
| §7.6 | Entry merge matrix (3x3) | N/A | No nested commits; coalescing in `LedgerDelta::record_*` handles intra-ledger flow |
| §7.10 | `CONFIG_SETTING` immutability | Full | delta.rs:376 |
| §8.1-8.3 | Upgrade types, validation, apply | Full | close.rs:1358, config_upgrade.rs:281 |
| §8.4 | Protocol-version side effects | Full | manager.rs:4395-4453 `create_*_for_v{20,21,22,23,25,26}` |
| §9.1 | Header update sequence | Full | manager.rs:4611 `build_and_hash_header` |
| §9.2 | Header validity | Full | common/header_validation.rs:21 |
| §9.3 | Skip-list construction | Full | header.rs:102 `calculate_skip_values` |
| §9.4 | Hash computation (live \|\| hot for P23+) | Full | header.rs:66 + manager.rs:2448-2470 |
| §10 | Soroban network configuration | Full | execution/config.rs, config_upgrade.rs |
| §10.4 | State-size sliding window | Full | manager.rs:5151-5213 |
| §11.1 | In-memory Soroban state, TTL co-located | Full | soroban_state.rs:316 |
| §11.2 | IMS update sequence (TTL → data → code) | Full | manager.rs:5221-5287 + soroban_state.rs:`process_entry_*` |
| §11.3 | State-size snapshot ordering (BEFORE flush) | Full | manager.rs:5168 (`soroban_state_size` computed before update) |
| §11.4 | Module cache lifecycle | Partial | manager.rs:2777 `rebuild_module_cache` — single-threaded, no `maybeRebuildModuleCache` heuristic |
| §11.5 | Hot archive restoration, disjoint maps | Full | execution/apply.rs:60 `RestoredEntries` with type-enforced disjointness |
| §12.1 | `sealLedgerTxnAndStoreInBucketsAndDB` | Partial | manager.rs:4757 (no `unsealHeader` callback — header restamped via `create_next_header` after delta drain) |
| §12.2 | Persistent state + HAS | Partial | App-layer DB write (out of ledger crate scope) |
| §13.1 | Meta version selection | Drift | manager.rs:5991 — always emits V2 regardless of `initialLedgerVers` |
| §13.2/13.3 | Meta contents and construction order | Full | execution/meta.rs:103,467 + manager.rs:5832 |
| §13.4 | Single `emitNextMeta` | N/A | Meta returned to caller; emission owned by `crates/app` |
| §14.1 | Genesis ledger constants | Full | manager.rs:6004 `create_genesis_header` |
| §14.2 | `startNewLedger` procedure | Partial | manager.rs:1762 `initialize` — accepts bucket lists, no root account synthesis |
| INV-L1 | Single-child LedgerTxn | N/A | Single flat delta |
| INV-L2 | Same-thread LedgerTxn access | N/A | No LedgerTxn handles to share |
| INV-L3 | Monotonic seq + hash chain | Full | manager.rs:2099, 2114 |
| INV-L4 | Total coins conservation | Absent | No `ConservationOfLumens` invariant runtime check (invariant/PARITY_STATUS.md:25) |
| INV-L5 | Restored entries mutual exclusion | Full | execution/apply.rs:60 (assertion-enforced) |
| INV-L6 | Sealed-after-commit | N/A | No seal state |
| INV-L7 | Fee pool non-negative | Full | common/header_validation.rs:22 (encode-time check) |
| INV-L8 | Phase-state safety | N/A | No ApplyState phase machine |
| INV-L9 | Header validity field bounds | Full | common/header_validation.rs:21 |
| INV-L10 | TxSet rooting | Full | app/ledger_close.rs:1897, 1962 |
| INV-L11 | Expected-hash check | Full | manager.rs:5536 |
| INV-L12 | Single SCP value per LCL | Full | app/ledger_close.rs:1804 (`is_applying_ledger` guard) + `try_start_ledger_close` next-seq gate |
| INV-L13 | HAS / LCL agreement on reload | Partial | manager.rs:1847 `verify_and_install_bucket_lists` checks bucket-list hash; seq-equality check delegated to app |
| INV-L14 | CONFIG_SETTING immutability | Full | delta.rs:376 |
| INV-L15 | Header re-seal must not modify entries | N/A | Header written via `create_next_header` after delta drain |

---

## Detailed Findings (by spec section)

### §3 — Data Types

- **§3.1 LedgerHeader field bounds.** Full. `header.rs:280` `create_next_header` calls `validate_header_fields` (common/header_validation.rs:21). All four canonical bounds enforced.
- **§3.3 InternalLedgerEntry / SPONSORSHIP / SPONSORSHIP_COUNTER / MAX_SEQ_NUM_TO_APPLY.** N/A. Spec calls out these as internal LedgerTxn wrappers; the Rust port stores plain `LedgerEntry` in `LedgerDelta`. The corresponding stellar-core invariants are about cross-LedgerTxn-seal visibility, not protocol output.
- **§3.5 LedgerCloseMeta v0/v1/v2 selection.** Drift. manager.rs:5991 unconditionally emits V2 (see Drift items below).
- **§3.7 RestoredEntries disjointness.** Full. execution/apply.rs:60 uses a single `HashMap<LedgerKey, RestoreSource>` and asserts no overlap in `insert_hot_archive_pair` / `insert_live_bl_pair`.

### §4 — Ledger Close Pipeline

- **§4.1 step 1-7.** Full. `manager.rs:2073` `begin_close` performs initialization-check, header load, seq increment (checked, manager.rs:2100), prev-hash set via `create_next_header` (manager.rs:4664). No explicit "stopping" check inside ledger crate — gated by app's `fatal_state_failure` (app/ledger_close.rs:1801).
- **§4.1 step 2 `finishPendingCompilation`.** Absent. No module-compile drain check at close entry; the cache is rebuilt eagerly at protocol-upgrade time (manager.rs:5694).
- **§4.2 step 8 protocol-version cap.** Full. manager.rs:2082-2097 panics on out-of-range version (matches "cannot apply ledger with not supported version").
- **§4.2 step 9 prev-hash equality.** Full. manager.rs:2114 (and pre-validated at app/ledger_close.rs:1897 with FATAL log).
- **§4.2 step 10 txset contents-hash.** Full. app/ledger_close.rs:1962 — done at app layer; ledger crate trusts `close_data.tx_set_hash()`.
- **§4.2 step 11 `scpValue` assignment.** Full. manager.rs:4669 — `tx_set_hash` and `close_time` written into `header.scp_value` via `NextHeaderFields`.
- **§4.2 step 12 `prepareForApply`.** Partial. app/ledger_close.rs:1982 validates pre-close but the ledger crate itself does not re-call it inside `begin_close`. Acceptable defense-in-depth boundary.
- **§4.4 step 14-15 (fee phase + per-source seq).** Full. manager.rs:4057 `pre_deduct_all_fees_on_delta` charges fees in a unified pre-pass; sequence-number advancement and pre-conditions are performed inside `TransactionExecutor::execute_transaction_with_fee_mode`. The `MAX_SEQ_NUM_TO_APPLY` marker (§4.4 step 15 / §6.2 step 7) is implemented at `execution/tx_set.rs:165,347-371` via `compute_max_seq_num_to_apply`.
- **§4.6 result-set hash.** Full. manager.rs:4768 — streamed sha256 of all `TransactionResult` entries.
- **§4.8 upgrades.** Full. close.rs:1358 `apply_to_header` handles `Version/BaseFee/MaxTxSetSize/BaseReserve/Flags`; manager.rs:4395 wraps each in capture/skip-on-error (parity with stellar-core's per-upgrade try/catch). `MaxSorobanTxSetSize` applied via close.rs:1412.
- **§4.9 seal + persist.** Partial. manager.rs:4757 `commit`:
  - drains `LedgerDelta` into init/live/dead vectors (no `getAllEntries` parent/child traversal — flat delta);
  - feeds `bucket_list.add_batch_unique`;
  - computes `bucket_list_hash`, builds new header via `create_next_header` (not via `unsealHeader` callback).
- **§4.10 meta finalization.** Partial. manager.rs:5832 builds V2 meta with `ext.v1.soroban_fee_write_1kb` when configured.
- **§4.11 8-step sequence after seal.** Partial. Step 1 (checkpoint queue) and step 6 (publish queued history) are app-layer; step 7 (forget unreferenced buckets) is implicit through bucket-list lifecycle; step 4 (background eviction scan) is **Full** (manager.rs:5511); ordering mostly correct but **commit_close (LCL publish) runs before meta build** which differs from spec §4.11 ordering (Drift, see Drift items).

### §5 — Apply State Phase Machine

- N/A. PARITY_STATUS.md:24 notes "no ApplyState phase machine". The single-threaded Rust port effectively spends the entire `close_ledger` in what stellar-core would call `APPLYING + COMMITTING`. INV-L8 not enforced because the underlying invariant has no representation.

### §6 — Transaction Application

- **§6.1 apply order.** Full. close.rs:603 `sorted_for_apply_sequential` xors `txSetHash` into account ordering; per-source seq strictly preserved.
- **§6.2 fee phase.**
  - Step 1-3 (per-tx fee charge, seq advance, result capture): Full — fee deducted via `LedgerDelta::deduct_fee_from_account` (delta.rs:435) and `execution::pre_deduct_all_fees_on_delta`.
  - Step 4 `accToMaxSeq` / `mergeSeen`: **Full**. `execution/tx_set.rs:347-371` `compute_max_seq_num_to_apply` scans all transactions for `AccountMerge` operations, builds a per-source max-sequence map, and installs it on the executor via `set_max_seq_num_to_apply`. Called at `tx_set.rs:165` during sequential-phase setup. Regression-tested by `test_audit_577_max_seq_num_to_apply_with_pre_charged`.
  - Step 7 `MAX_SEQ_NUM_TO_APPLY` synthesis: **Full** (same function at tx_set.rs:347-371).
- **§6.3 phase selection.** Full. manager.rs:4044 dispatches to `execute_soroban_parallel_phase` when V1 Soroban phase present.
- **§6.4 sequential phase.** Full. `applySequentialPhase` steps 1-7 are inlined in `execution::run_transactions_on_executor` (execution/tx_set.rs:122) with `prepend_fee_event` for classic events (manager.rs:4218).
- **§6.5 parallel Soroban phase.**
  - Stages applied serially: Full (execution/tx_set.rs:803 stage loop).
  - Clusters applied concurrently: Full (execution/tx_set.rs:1350 `execute_stage_clusters` via tokio).
  - `flushRoTTLBumpsInTxWriteFootprint`: Full (TTL coalescing via `merge_ttl_current` in delta.rs:323).
  - `checkAllTxBundleInvariants`: Absent (no `invariant::InvariantManager` per-bundle hook in parallel path; manager-level check at execution/mod.rs:802 only fires per-op in classic).
  - `commitChangesToLedgerTxn`: Full (results flow back into outer `LedgerDelta` via `delta` &mut argument).
- **§6.6 post-tx-set apply.** Full. execution/tx_set.rs:242 + execution/tx_set.rs:902 populate `post_tx_apply_fee_processing` for V2 meta.
- **§6.7 prefetch.** Full. execution/tx_set.rs:728 `snapshot.prefetch(&keys_vec)` is advisory only.

### §7 — LedgerTxn Nested Transactional State

- All structural claims (§7.1 hierarchy, §7.2 INIT/LIVE/DELETED, §7.3 mActive, §7.4 sealing, §7.5 commit/rollback, §7.6 entry merge matrix, §7.8 getChanges/getDelta) are **N/A**. The Rust port replaces nested `LedgerTxn` chains with a single `LedgerDelta` plus snapshot read-through. PARITY_STATUS.md:35 acknowledges this as an intentional simplification.
- **§7.7 last-modified stamping.** Full. `LedgerDelta::record_create`/`record_update` keep `last_modified_ledger_seq` on the entry as written; v23/v25/v26 upgrade synthesizers (manager.rs:3523, 3536, etc.) all set `last_modified_ledger_seq = ledger_seq`.
- **§7.10 config setting non-erasable.** Full. delta.rs:376 returns `InvalidEntry("cannot delete ConfigSetting entries")` and a test (delta.rs:1668) confirms.

### §8 — Protocol and Network Upgrades

- **§8.1 upgrade types.** Full. close.rs:1358 covers all seven `LedgerUpgrade` variants; `MaxSorobanTxSetSize` handled separately in close.rs:1412 by patching `ContractExecutionLanes`.
- **§8.2 `isValidForApply` (XDR_INVALID / INVALID / VALID).** Full. config_upgrade.rs:281 returns `ConfigUpgradeValidity::{XdrInvalid, Invalid, Valid}`.
- **§8.3 nested LedgerTxn around each upgrade + catch on throw.** Full (substituted form). manager.rs:4407, 4471, 4507, 4543 each wrap an upgrade in `ltx.capture_entry_changes(|ltx| ...)` with per-upgrade try/catch (`Ok / Err` arms log + skip).
- **§8.4 protocol-version side effects.** Full. manager.rs covers `create_ledger_entries_for_v20`, `create_cost_types_for_v21`, `_v22`, `create_and_update_ledger_entries_for_v23`, `_v25`, `update_cost_types_for_v26`, `create_ledger_entries_for_v26` (manager.rs:3512, 3666, 3739, 3789).
- **§8.4 P23→P24 `p23_hot_archive_bug`.** Partial. manager.rs:5417-5454 gates hot-archive `add_batch` on `prev_version >= V23` (matching stellar-core's `initialLedgerVers` check), but the `Protocol23CorruptionDataVerifier` is not implemented (operational concern for production network only).
- **§8.4 `handleUpgradeAffectingSorobanInMemoryStateSize`.** Full. manager.rs:4422 calls `handle_upgrade_affecting_soroban_state_size` inside the V23 upgrade scope.

### §9 — Ledger Header Management

- All five sub-sections Full. Skip-list algorithm in header.rs:102 matches the cascade described in §9.3 (verified via Appendix C example).
- Bucket-list-hash construction: header.rs:122 sets `skip_list[0] = bucket_list_hash` after cascade; protocol-23+ live||hot combination at manager.rs:5460-5470 (matches §9.4).

### §10 — Soroban Network Configuration

- Config loading: Full. execution/config.rs provides `load_soroban_config`, `load_soroban_network_info`, `require_soroban_config`, `load_state_archival_settings`, `load_frozen_key_config`.
- Per-protocol upgrade synthesis: Full (see §8.4).
- §10.4 sliding window: Full. manager.rs:5151 invokes `compute_state_size_window_entry` only on sample ledgers; sample is taken from in-memory state BEFORE this ledger's writes (§11.3 ordering).

### §11 — Soroban State Management

- **§11.1 InMemorySorobanState layout.** Full. soroban_state.rs:316 keyed by TTL key hash; TTL data folded inline into data/code entries.
- **§11.2 update sequence.** Full. manager.rs:5233-5278 walks init then live then dead; module cache evicts on contract-code deletion (manager.rs:5266).
- **§11.3 state-size snapshot order.** Full. manager.rs:5170 reads `soroban_state.read().total_size()` BEFORE the `process_entry_*` loop, so the sample captures end-of-prior-ledger state.
- **§11.4 module cache.** Partial. `rebuild_module_cache` is single-threaded; no shared multi-threaded compiler (PARITY_STATUS.md:48). `maybeRebuildModuleCache` heuristic absent — cache rebuild is unconditional on cross-protocol upgrade (manager.rs:5693).
- **§11.5 hot-archive restoration.** Full. execution/apply.rs:60 `RestoredEntries` enforces hot-archive ⊕ live-BL disjointness via `RestoreSource` enum (no key can simultaneously be `HotArchive` and `LiveBucketList`); assertions panic on conflict.

### §12 — Commit and Persistence

- **§12.1 `sealLedgerTxnAndStoreInBucketsAndDB`.** Partial. manager.rs:4757 covers the equivalents:
  - eviction scan resolve + add_hot_archive_batch (5009-5135);
  - meta evicted-keys population (5110);
  - module cache update (5267, 5694);
  - state-size snapshot (5151);
  - final-soroban-config load (4826);
  - getAllEntries equivalent via `drain_for_bucket_update` (4867);
  - add_live_batch via `bucket_list.add_batch_unique` (5375).
  - **Missing**: `unsealHeader(f)` callback contract. Header is built fresh via `create_next_header` (4664) AFTER the bucket list hash is known; same observable result but the spec's "re-unseal" mechanism is not present.
- **§12.2 persistent state + HAS.** Partial. Persistence and the HAS table write live in `crates/app` (DB layer). Ledger crate does NOT persist HAS or header to DB; it returns `LedgerCloseResult` to the caller.
- **§12.3 LCL state.** Full. manager.rs:2553 `commit_close` atomically swaps the header + hash + cached soroban network info.

### §13 — Ledger Close Meta

- **§13.1 version selection.** Drift. manager.rs:5991 unconditionally emits `LedgerCloseMeta::V2` (see comment at manager.rs:5988-5990 which acknowledges the spec branch but justifies the divergence by Henyey's protocol-24-only support). Acceptable since the project's `MIN_LEDGER_PROTOCOL_VERSION` is 24, but the conditional is not present, so a hypothetical run against an older history would mis-emit.
- **§13.2 contents.** Full. manager.rs:5832 includes all V2 fields.
- **§13.3 construction order.** Full. `populateTxSet` → `pushTxFeeProcessing` (execution/meta.rs:`prepend_fee_event` deferred until post-exec at manager.rs:4218) → `setTxProcessingMetaAndResultPair` (execution/result_mapping.rs) → `setPostTxApplyFeeProcessing` (execution/tx_set.rs:280) → `upgradesProcessing` (manager.rs:5832 inputs) → `evictedKeys` (manager.rs:5110) → `ledgerHeader` (manager.rs:5967).
- **§13.4 emission.** N/A. Meta is returned in `LedgerCloseResult`; emission is owned by `crates/app/src/app/persist.rs`.

### §14 — Genesis Ledger

- **§14.1 constants.** Full. manager.rs:6004 `create_genesis_header` matches all six constants from §14.1 (Spec anchor at 6016 references "§13.1" — see Dangling anchors).
- **§14.2 `startNewLedger` procedure.** Partial. manager.rs:1762 `initialize` accepts pre-built bucket lists and a header; it does not synthesize the root `AccountEntry` from `SecretKey::fromSeed(networkID)`. The root account is expected to come from the bucket list (i.e. the catchup pipeline rather than a fresh genesis). For a true `startNewLedger`, a caller must build the entries before invoking `initialize`.
- **§14.3 subsequent initialization.** Full. manager.rs:1995 `initialize_all_caches` re-builds Soroban state + module cache after a bucket-list reload.

### §15 — Invariants

| Invariant | Status | Enforcement |
|---|---|---|
| INV-L1 | N/A | Single flat `LedgerDelta`; no parent/child notion. |
| INV-L2 | N/A | `CloseLedgerState` is not `Send` cross-thread inside `close_ledger`; entire close runs on one thread. |
| INV-L3 | Full | manager.rs:2099 (checked seq), manager.rs:2114 (prev-hash equality). |
| INV-L4 | **Absent** | `crates/invariant/PARITY_STATUS.md:25` confirms `ConservationOfLumens` is unimplemented. Runtime sum-of-balances == total_coins check not performed. |
| INV-L5 | Full | execution/apply.rs:60 `RestoredEntries` — assertion-enforced disjoint maps. |
| INV-L6 | N/A | No seal state. |
| INV-L7 | Full | common/header_validation.rs:22; called from `create_next_header` (header.rs:318). |
| INV-L8 | N/A | No `ApplyState` phase machine; `crates/ledger` is single-writer by virtue of `parking_lot::RwLock` around `state`. |
| INV-L9 | Full | common/header_validation.rs:21. |
| INV-L10 | Full | app/ledger_close.rs:1897 (prev-hash) + 1962 (contents-hash). |
| INV-L11 | Full | manager.rs:5536-5667 — pre-commit check with detailed diagnostic dump on mismatch. |
| INV-L12 | Full | app/ledger_close.rs:1804 `is_applying_ledger` mutex; ledger seq monotonicity at manager.rs:2106 enforces "exactly one ledger per seq". |
| INV-L13 | Partial | manager.rs:1847 verifies bucket-list hash against header. The HAS↔LCL `ledger_seq` agreement check on reload is owned by `crates/app` (load_last_known_ledger). |
| INV-L14 | Full | delta.rs:376. |
| INV-L15 | N/A | No `unsealHeader` callback; header constructed after delta drain rather than re-opened. |

---

## Dangling Spec Anchors

These `// LEDGER_SPEC §X` comments cite section numbers that have moved or are imprecise in the regenerated v26.0.1 spec:

| File:Line | Cited Section | Actual Topic in Spec | Suggested Fix |
|---|---|---|---|
| `crates/ledger/src/manager.rs:6016` | `LEDGER_SPEC §13.1` (genesis constants) | §13.1 is meta selection; genesis is §14.1 | Rewrite to `§14.1` |
| `crates/ledger/src/manager.rs:7285` | `LEDGER_SPEC §13.1` (genesis fields) | Same as above | Rewrite to `§14.1` |
| `crates/ledger/src/manager.rs:5820` | `LEDGER_SPEC §12.2 — meta version selection` | §12.2 is persistent state + HAS; meta selection is §13.1 | Rewrite to `§13.1` |
| `crates/ledger/src/manager.rs:5950` | `LEDGER_SPEC §12.2 — meta version selection` | Same as above | Rewrite to `§13.1` |
| `crates/ledger/src/manager.rs:5988` | `LEDGER_SPEC §12.2 / §15.11` | §15 has invariants by ID, no §15.11 | Rewrite to `§13.1 / INV-L9` |
| `crates/ledger/src/manager.rs:4744` | `LEDGER_SPEC §4.2 / §15.13 / Appendix E` | §4.2 is validation, not commit; no §15.13; no Appendix E | Rewrite to `§4.9 / §12.1 / Appendix B` |
| `crates/ledger/src/manager.rs:3888` | `LEDGER_SPEC §5.2 — transaction application` | §5 is apply-state machine; tx application is §6 | Rewrite to `§6` |
| `crates/ledger/src/manager.rs:2886` | `LEDGER_SPEC §2.3 / Appendix C — apply state phases` | §2 has no §2.3; apply-state phases are §5; Appendix C is skip-list | Rewrite to `§5 (N/A in henyey)` |
| `crates/ledger/src/manager.rs:2069` | `LEDGER_SPEC §4.1 — value externalized entry point` | §4.1 covers entry/seq increment; valueExternalized terminology comes from §2 / CATCHUP_SPEC §6 | Acceptable; tighten to `§4.1` |
| `crates/ledger/src/manager.rs:2099` | `LEDGER_SPEC §15.2` (overflow) | §15 has no §15.2; this is INV-L9 (header validity) | Rewrite to `INV-L9` |
| `crates/ledger/src/manager.rs:4607` | `LEDGER_SPEC §3.3 — ledger header construction` | §3.3 is `InternalLedgerEntry`; header construction is §9.1 | Rewrite to `§9.1` |
| `crates/ledger/src/manager.rs:4618` | `LEDGER_SPEC §15` (checked arithmetic) | OK in spirit; §15 covers header validity invariants | Tighten to `INV-L9` |
| `crates/ledger/src/header.rs:317` | `LEDGER_SPEC §15` | OK — §15 is the invariants section, encompassing INV-L9 | Tighten to `INV-L9` |

13 anchors total; all are imprecise/dangling but none reference fully-removed content. A mechanical pass updating the section numbers would clear them.

---

## Drift Items (require human review)

1. **§13.1 meta version selection** (manager.rs:5991). Spec mandates v0/v1/v2 selection by `initialLedgerVers`; Rust always emits V2. Justified by the project's `MIN_LEDGER_PROTOCOL_VERSION=24` (which forces v2) but a malformed config or replay scenario at < P20 would emit a wrong-shaped meta. **Recommendation**: gate on `initialLedgerVers` and emit v0/v1/v2 conditionally, even if unreachable in production.
2. **§4.11 step 8 ordering — LCL publish before meta build** (manager.rs:5680 then 5832). Spec orders meta finalization (§4.10 step 26 `emitNextMeta`) BEFORE the 8-step post-seal sequence and BEFORE `commit_close` (which equates to spec step 8 `ledgerCloseComplete → lastClosedLedgerIncreased`). Rust commits the new header (manager.rs:5680) and THEN builds the meta (5832). External readers can therefore observe `LCL = N` before meta for N is emitted; downstream consumers tolerant of out-of-order emission (per §13.4 last paragraph) handle this, but it diverges from the spec's prescribed order.
3. **§12.1 `unsealHeader(f)` contract** (manager.rs:4757). Spec mandates header re-opening via `unsealHeader` with a callback that only mutates header fields; Rust builds the header fresh after delta drain. Same observable hash, but the safety property INV-L15 ("header re-seal must not modify entries") is **vacuously** satisfied in henyey — there is no callback to validate.

---

## Absent — Correctness-Relevant Gaps

1. **INV-L4 `ConservationOfLumens`.** Not enforced at runtime. The downstream invariant crate has a stub (crates/invariant/PARITY_STATUS.md:25). Without it, an arithmetic bug in fee/refund/inflation paths could silently mint or burn XLM.
2. **§4.1 step 2 `finishPendingCompilation`.** No drain of pending Wasm compilations at close entry. Rust's `rebuild_module_cache` runs synchronously at upgrade boundaries (manager.rs:5694), so the failure mode (close starts while a compile thread is mid-flight) doesn't exist in the current single-threaded compile design — but if multi-threaded compile is added later this gap matters.
3. **§6.5 `checkAllTxBundleInvariants`.** Parallel-cluster post-apply invariant hook absent (search: `grep checkAllTxBundleInvariants` and `grep tx_bundle_invariant` both empty). The single-tx invariant manager (execution/mod.rs:802) only fires per-op in classic path.
4. **§14.2 root `AccountEntry` synthesis.** `initialize` does not create the network-id-derived root account; this must be done by the caller (typically by feeding genesis buckets). Replay/test flows that bypass the catchup-from-buckets path will not get a root account.
5. **§12.2 P23 `Protocol23CorruptionDataVerifier`.** Optional in spec; not implemented. Acceptable for parity but flagged for completeness.

---

## Recommendations

1. **High priority — INV-L4.** Implement `ConservationOfLumens` runtime check; protocol-deterministic safety property.
2. **Medium — §13.1 Drift.** Gate meta version on `initialLedgerVers` even if `MIN_LEDGER_PROTOCOL_VERSION=24`; cheap defense-in-depth.
3. **Medium — §4.11 step ordering.** Re-order `commit_close` to occur AFTER `build_ledger_close_meta` to match the spec sequence and INV-L13 expectations.
4. **Low — dangling anchors.** A mechanical pass renumbering 13 `// Spec: LEDGER_SPEC §...` comments to the regenerated v26.0.1 section numbers (table above).
5. **Low — §14.2 `startNewLedger`.** Provide a `start_new_ledger(network_id) -> Result<()>` helper that synthesizes the genesis bucket lists with the network-id-derived root account.
