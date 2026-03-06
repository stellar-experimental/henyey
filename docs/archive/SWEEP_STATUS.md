# Verify-Execution Sweep Status

> **Updated**: 2026-03-03 22:45 UTC
> **Session**: b5e87aee (fresh start)
> **CDP data lake range**: L59501248–L61366079 (latest available as of 2026-02-23)
> **Supported protocol**: P24+ (L59501312 is first P24 ledger; L59501248–L59501311 are P23 and unverifiable)

## Protocol boundary

Protocol 24 activated at **L59501312** (exactly 64 ledgers after the cache start at L59501248).
Ledgers L59501248–L59501311 (P23) cannot be verified by Henyey (min supported: P24).

## Verified ranges

| Range | Status | Notes |
|-------|--------|-------|
| L59501248–L59501311 | **SKIPPED** | Protocol 23 — Henyey does not support P23 |
| L59501312–L59511311 | **CLEAN** | s1 completed — 10,000 ledgers, 0 mismatches |
| L59511312–L59521311 | **CLEAN** | s2 completed — 10,000 ledgers, 0 mismatches |
| L59521312–L59531311 | **CLEAN** | s3 completed — 10,000 ledgers, 0 mismatches |
| L59531312–L59541311 | **CLEAN** | s4 completed — 10,000 ledgers, 0 mismatches (VE-07 fix applied) |
| L59541312–L59551311 | **CLEAN** | s5 completed — 10,000 ledgers, 0 mismatches |
| L59551312–L59561311 | **CLEAN** | s6 completed — 10,000 ledgers, 0 mismatches |
| L59561312–L59571311 | **CLEAN** | s7 completed — 10,000 ledgers, 0 mismatches |
| L59571312–L59581311 | **CLEAN** | s8 completed — 10,000 ledgers, 0 mismatches |
| L59581312–L59591311 | **CLEAN** | s9 completed — 10,000 ledgers, 0 mismatches |
| L59591312–L59601311 | **CLEAN** | s10 completed — 10,000 ledgers, 0 mismatches |
| L59601312–L59611311 | **CLEAN** | s11 completed — 10,000 ledgers, 0 mismatches |
| L59611312–L59621311 | **CLEAN** | s12 completed — 10,000 ledgers, 0 mismatches |
| L59621312–L59631311 | **CLEAN** | s13 completed — 10,000 ledgers, 0 mismatches |
| L59631312–L59641311 | **CLEAN** | s14 completed — 10,000 ledgers, 0 mismatches (VE-08 fix applied) |
| L59641312–L59741311 | **CLEAN** | s15 completed — 100,000 ledgers, 0 mismatches |
| L59741312–L59841311 | **CLEAN** | s16 completed — 100,000 ledgers, 0 mismatches |
| L59841312–L59941311 | **CLEAN** | s17 completed — 100,000 ledgers, 0 mismatches |
| L59941312–L60041311 | **CLEAN** | s18 completed — 100,000 ledgers, 0 mismatches |
| L60041312–L60141311 | **CLEAN** | s19 completed — 100,000 ledgers, 0 mismatches |
| L60141312–L60241311 | **CLEAN** | s20 completed — 100,000 ledgers, 0 mismatches |
| L60241312–L60341311 | **CLEAN** | s21 completed — 100,000 ledgers, 0 mismatches |
| L60341312–L60441311 | **CLEAN** | s22 completed — 100,000 ledgers, 0 mismatches |
| L60441312–L60541311 | **CLEAN** | s23 completed — 100,000 ledgers, 0 mismatches |
| L60541312–L60641311 | **CLEAN** | s24 completed — 100,000 ledgers, 0 mismatches |
| L60641312–L60741311 | **CLEAN** | s25 completed — 100,000 ledgers, 0 mismatches (VE-10 fix applied) |
| L60741312–L60841311 | **CLEAN** | s26 completed — 100,000 ledgers, 0 mismatches |
| L60841312–L60941311 | **CLEAN** | s27-rerun completed — 100,000 ledgers, 0 mismatches (VE-11 fix applied) |
| L60941312–L61041311 | **CLEAN** | s28 completed — 100,000 ledgers, 0 mismatches |
| L61041312–L61141311 | **CLEAN** | s29 completed — 100,000 ledgers, 0 mismatches |
| L61141312–L61241311 | **CLEAN** | s30 completed — 100,000 ledgers, 0 mismatches |
| L61241312–L61341311 | **CLEAN** | s31 completed — 100,000 ledgers, 0 mismatches |

## Previously confirmed bug fixes (from prior sessions)

- **VE-01**: Snapshot overwrite in `update_*` methods — fixed in `0fb052d`, `d482c43`
- **VE-02**: Pool share trustlines not loaded from bucket list — fixed in `710ae8d`
- **VE-03**: Fee-bump inner sig check ordering — fixed in `acf4472`
- **VE-04**: `check_signature_from_signers` 0>=0 bug — fixed in `3930486`
- **VE-05**: Spurious TTL INIT for read-only archived entries — fixed in `16933b2`
- **VE-06**: Hot archive keys collected for failed operations — fixed in `e8d22fa` + `7a172d7`

## Bug fixes (this session)

- **VE-07**: PreAuthTx signer removed before signature check — fixed in `f6948ba`
  - **Ledger**: L59531878, TX 91 (hash `9901cec6...`)
  - **Symptom**: txBAD_AUTH (ours) vs txSuccess (CDP) for a Payment with no signatures
  - **Root cause**: `remove_one_time_signers_from_all_sources` ran BEFORE `check_operation_signatures`. A prior TX in the same ledger added a PreAuthTx signer for this TX's hash, but our code consumed it before the signature check could see it.
  - **Fix**: Move signature checking to before one-time signer removal, matching stellar-core's ordering (checkAllTransactionSignatures → processSeqNum → checkOperationSignatures → removeOneTimeSignerFromAllSourceAccounts).

- **VE-08**: Signature check skipped for Soroban transactions — fixed in `dc2d81b`
  - **Ledger**: L59638512, TX 171 (hash `d9096fb5...`)
  - **Symptom**: txFeeBumpInnerSuccess (ours) vs txFeeBumpInnerFailed/TxBadAuth (CDP) for a fee-bump wrapping InvokeHostFunction. Fee pool off by 5782 stroops.
  - **Root cause**: The `!frame.is_soroban()` guard on the apply-time signature check skipped checking for all Soroban transactions. For fee-bump Soroban TXs, a prior TX in the same ledger modified the inner source's signer set, invalidating the inner signatures. stellar-core's `processSignatures()` calls `checkOperationSignatures()` for all transaction types.
  - **Fix**: Remove the `!frame.is_soroban()` guard so signature checking runs for all transaction types, matching stellar-core's behavior.

- **VE-09**: LedgerDelta merge rejects parallel cluster double-delete — fixed in `9e901ca`
  - **Ledger**: L61403915 (tracker catchup)
  - **Symptom**: Tracker crashed during catchup with "invalid merge: delete on deleted entry" in `LedgerDelta::merge`.
  - **Root cause**: Parallel Soroban clusters can independently delete the same entry (e.g. a TTL key present in multiple footprints). The merge logic rejected this as invalid, despite within-delta double-deletes already being handled as no-ops.
  - **Fix**: Make delete-on-deleted idempotent in the merge path, matching the within-delta behavior at line 383.

- **VE-10**: Deleted account reloaded from snapshot in parallel fee path — fixed in `07fbf59` + `0782a40`
  - **Ledger**: L60645316, TX 68 (hash `050055a3...`, account GBPHB57...)
  - **Symptom**: txSuccess (ours) vs TxNoAccount (CDP). Also fee=0 vs fee=100.
  - **Root cause**: In the parallel fee-deduction path, accounts are bulk-loaded into executor state via `state.load_entry()` (bypassing `load_account()`), so the `loaded_accounts` guard is never populated. When a prior TX in the ledger deletes the account via account_merge, a subsequent TX's `load_account` fell through to the bucket-list snapshot and returned the stale LIVE entry.
  - **Fix 1** (`07fbf59`): Add `delta().deleted_keys()` check to `load_account()` and `load_account_without_record()`, matching the existing pattern in `load_trustline` and `load_claimable_balance`.
  - **Fix 2** (`0782a40`): Override `result.fee_charged` with the pre-charged fee for validation failures in the parallel path, since the executor runs with `deduct_fee=false`.

- **VE-11**: Deleted entries not propagated across parallel Soroban stages — fixed in `4ca973c`
  - **Ledger**: L61430400, TX 331 (InvokeHostFunction)
  - **Symptom**: txSuccess (ours) vs Trapped (CDP) for an InvokeHostFunction in stage 1. Hash mismatch on ledger close.
  - **Root cause**: In parallel Soroban execution, entries deleted within stage 0 (via apply_soroban_storage_changes when host doesn't return a new value for an RW footprint entry) were not propagated to stage 1. `delta.current_entries()` only returns Created+Updated entries, omitting deletions. A fresh executor in stage 1 would reload the deleted entry from the bucket list via `load_soroban_footprint`, causing the host to succeed where it should have trapped.
  - **Fix**: Propagate `delta.dead_entries()` as `prior_stage_deleted_keys` to subsequent stage clusters, and call `mark_entry_deleted()` to populate the deleted sets in the fresh executor. This matches stellar-core's `cleanEmpty` propagation via `collectClusterFootprintEntriesFromGlobal`.

## Bug fixes (this session, continued)

- **VE-12**: Corrupt permanent bucket file from disk-full partial write — fixed in `88743e3`
  - **Ledger**: L61341704 (triggered by disk-full at L61341701)
  - **Symptom**: `L1.curr.hash() = e3b0c44...` (SHA-256 of empty string) after ledger close
  - **Root cause**: Two-step failure chain:
    1. `save_to_xdr_file` for in-memory bucket (hash `2415f6ce...`) — `File::create` succeeded but `write_all` failed with `ENOSPC`, leaving a zero-byte permanent file.
    2. At L61341702, `merge(empty_curr, bucket_2415f6ce...)` → output hash = `2415f6ce...` → `start_merge` finds the existing permanent file, skips the rename, loads zero-byte file → `from_xdr_file_disk_backed` produces SHA-256("") = `e3b0c44...`. At L61341704, `commit(L1)` resolves this corrupt merge → `L1.curr.hash() = e3b0c44...`.
  - **Fix 1** (`88743e3`): `save_to_xdr_file` now deletes the partial file if `write_all` or `sync_all` fails.
  - **Fix 2** (`88743e3`): `start_merge` verifies that an existing permanent file's hash matches the expected output; replaces it if corrupt.

## Infrastructure incident (2026-03-02)

The original s32 run (started 2026-03-02 13:55 UTC) hit **disk full** at L61341704:
- Background merges at levels 8, 9, 10 failed with `No space left on device`
- Merge failure left corrupted bucket state → hash mismatch at L61341704
- This was NOT a code bug; disk was filled by accumulated sweep caches (cache/, cache-2/)
- Resolution: deleted stale sweep caches (cache-2/ 1.2T, cache/ 194G), freeing 1.4T
- s32 restarted clean from L61341312 on 2026-03-03

## Running sweeps

| Sweep | Range | Status | Started |
|-------|-------|--------|---------|
| s32-rerun | L61341312–L61366079 | running | 2026-03-03 22:42 UTC |

## Tracker

| Status | PID | Started |
|--------|-----|---------|
| Live | 2958934 | 2026-03-04 21:30 UTC (restarted with latest binary; HTTP port changed to 11727 to avoid conflict with quickstart stellar-core on 11626) |

## Cache scan benchmark (2026-03-01)

Measured on 63 GB machine with two resident sweepers (~32 GB RSS total).

| N | Wall time | Peak RSS (tracker) | Result |
|---|-----------|-------------------|--------|
| 2 | ~95 s (est) | 23.2 GB | OOM-killed (55 GB total; 8 GB short of limit) |
| 1 | 172 s | 18.8 GB | Stable |

Per-level scan times (N=1, sequential):

| Level | Entries | TTLs | Scan time |
|-------|---------|------|-----------|
| 0 | 1,600 | 1,221 | 5 ms |
| 1 | 4,543 | 2,450 | 17 ms |
| 2 | 12,671 | 7,929 | 65 ms |
| 3 | 29,142 | 24,757 | 201 ms |
| 4 | 90,355 | 81,630 | 405 ms |
| 5 | 301,687 | 287,640 | 1.4 s |
| 6 | 1,389,033 | 1,366,166 | 11.2 s |
| 7 | 2,369,305 | 2,332,710 | 27.3 s |
| 8 | 2,580,343 | 2,507,783 | 41.0 s |
| 9 | 3,370,899 | 3,274,207 | 37.0 s |
| 10 | 1,956,994 | 969,184 | 43.6 s |

N=2 is safe when machine has >20 GB headroom above current sweeper RSS.
Default is N=1 (b2f6c87); scan_thread_count parameter is configurable.
