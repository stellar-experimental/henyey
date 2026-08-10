# CATCHUP_SPEC Adherence — henyey-history (+ henyey-historywork)

**Spec version:** 26 (stellar-core v26.0.1 / Protocol 26)
**Crate:** crates/history (with crates/historywork)
**Last updated:** 2026-05-20
**Overall adherence:** 90% (Full 65 | Partial 6 | Absent 1 | Drift 1 | N/A 7)

Sections §7 (LedgerApplyManager), §8.4 (apply-buffered drain),
INV-C8, and INV-C12 live in `crates/app` — they are marked N/A here
with pointers, per the spec-adhere rubric for cross-crate
invariants. INV-C9 (bucket-apply newest-wins) lives in
`crates/bucket` and is also marked N/A with a pointer.

---

## Summary table

| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §3.1 | HAS field layout | Full | `archive_state.rs:280-307` |
| §4.2 | Path construction `<NN>/<NN>/<NN>` | Full | `paths.rs:47-94` |
| §4.3 | `CHECKPOINT_FREQUENCY = 64` | Full | `checkpoint.rs:22` |
| §4.3 | `checkpointContainingLedger` | Full | `checkpoint.rs:68-71` |
| §4.3 | `firstLedgerInCheckpointContaining` | Full | `checkpoint.rs:166-173` |
| §4.3 | `sizeOfCheckpointContaining` | Full | `checkpoint.rs:267-274` |
| §4.3 | Ledger-zero pseudo-checkpoint | Full | `paths.rs:149-151`, `lib.rs:689-760` |
| §4.4 | HAS structural validation (level count, version, passphrase) | Full | `verify.rs:709-761` |
| §4.4 | BucketList version monotonicity + `next` rules | Full | `archive_state.rs:404-473` |
| §4.4 | `MAX_HISTORY_ARCHIVE_BUCKET_SIZE = 100 GB` | Full | `archive_state.rs:23`, enforced at `catchup/download.rs:230` |
| §4.5 | BucketList hash computation | Full | `archive_state.rs:600-654` |
| §5.1–§5.2 | CheckpointBuilder dirty→final rename | Full | `checkpoint_builder.rs:344-481` |
| §5.3 | HAS publish queue | Drift | `publish_queue.rs` — documented intentional implementation difference (see §5.3 drift item below) |
| §5.4 | MAX_PUBLISH_DELETE_CHECKPOINTS=100, delete_published_files | Full | `publish.rs:71-124` |
| §5.5 | Differing-buckets diff algorithm | Full | `archive_state.rs:220-274` |
| §5.6 | Publish queue backpressure (8/16) | Full | `publish_queue.rs:108-112`, `catchup/replay.rs:559-585` |
| §5.7 | restoreCheckpoint / cleanup recovery | Full | `checkpoint_builder.rs:491-680` |
| §6.1 | Catchup modes (OFFLINE_BASIC/OFFLINE_COMPLETE/ONLINE) | Partial | `catchup_range.rs` — `CatchupRunMode` enum + `CatchupConfiguration` wrapper; threaded through all entry points (#2829). OFFLINE_COMPLETE tx-result verification semantics deferred to #2831 (structural only until then) |
| §6.3 | calculateCatchupRange | Partial | `catchup_range.rs:221-319` — see Drift item below |
| §7 | LedgerApplyManager | N/A | lives in `crates/app/src/app/ledger_close.rs` |
| §8.1 | Phase 1: Fetch HAS | Full | `catchup/mod.rs:578-616` |
| §8.2 | Phase 2: Download + verify chain | Full | `catchup/mod.rs:540-545`, `verify.rs:253-441` |
| §8.4 | Apply buffered ledgers | N/A | lives in `crates/app/src/app/ledger_close.rs:1758-1792` (`try_apply_buffered_ledgers`) |
| §8.5 | Post-bucket asserts (HAS/header/LCL) | Full | `catchup/mod.rs:463-486` |
| §9.1 | Trust establishment | Full | `verify.rs:303-306` — `TrustSource::Scp` plumbed from `App::resolve_scp_trust_anchor()` through `CatchupManager::set_trusted_scp_anchor()` to `catchup/replay.rs` |
| §9.2 | Reverse-walk verification | Full | `verify.rs:253-441` |
| §9.3 | Outcome → fatal mapping | Full | `verify.rs:422-436` |
| §10.1 | Bucket size limit + per-bucket SHA-256 | Full | `catchup/download.rs:230-240`, bucket hash verify in `apply_buckets` (`catchup/buckets.rs:381-405`) |
| §10.2 | `containsValidBuckets` post-restore | Full | `catchup/buckets.rs:447-456` |
| §10.3 | Bucket apply algorithm | N/A | applies via `crates/bucket` BucketList replay (cross-crate) |
| §10.4 | Index buckets (live + hot) | Full | indirect via `bucket_list.restore_from_has` → `crates/bucket` |
| §10.5 | AssumeState | Full | `catchup/buckets.rs:154-216` (`restart_merges`) |
| §11.1 | Per-checkpoint replay | Full | `catchup/replay.rs:460-552` |
| §11.2 | Five-case knit-to-LCL | Full | `catchup/replay.rs:58-117` |
| §11.3 | txSet hash check (case 4) | Full | `verify.rs:596-631` (`verify_tx_set`), called at `catchup/replay.rs:248-249` |
| §11.4 | OFFLINE replay backpressure | Full | `catchup/replay.rs:480-483, 559-585` |
| §11.5 | Empty-ledger gap handling | Full | tx_set synthesized when entry absent (`catchup/replay.rs:244-248`) |
| §12 | OFFLINE_COMPLETE tx-results verify | Partial | `verify.rs:542-563` exists but is not gated by mode (always available); see §6.1 |
| §13 | Buffered drain | N/A | `crates/app` — see §8.4 |
| §14.2 | Archive rotation | Full | `catchup/mod.rs:369`, `catchup/download.rs:120` |
| §14.3 | Fatal failure flag | N/A | `fatal_state_failure` in `crates/app/src/app/mod.rs:557` |
| §14.4 | Publish-side crash recovery | Full | `checkpoint_builder.rs:491-680` |
| §14.5 | Catchup-side crash recovery | Partial | Two-window design: non-authoritative pre-LCL writes + `CATCHUP_PERSIST_PENDING` sentinel; `verify_on_disk_integrity()` is safe (hash chain valid for orphaned rows); CLI readers still anchor on `MAX(ledgerseq)`; see Detailed §14.5 |
| §16 | Constants table | Full | all values match (see Constants below) |

---

## Detailed findings (by section)

### §3.1 — HAS data type
- **Claim §3.1-1** (MUST): `version ∈ {1,2}`, `currentBuckets` has exactly `LIVE_BUCKETLIST_LEVELS`, `hotArchiveBuckets` present iff version ≥ 2.
- **Rust**: `crates/history/src/verify.rs:709-761`
- **Status**: Full. Both directions enforced — v1 must not contain `hotArchiveBuckets`, v2 must contain it. Also enforces `networkPassphrase` required at v≥2.

### §4.2 — Path Construction
- **Claim §4.2-1** (MUST): paths split as `<NN>/<NN>/<NN>/...-<LSEQ>.xdr.gz` where `LSEQ = lowercaseHex8(L)`.
- **Rust**: `crates/history/src/paths.rs:47-94`
- **Status**: Full. Tests for ledger 63, 127, 100 all match spec.

### §4.3 — Checkpoint Frequency / Math
- **Claims §4.3-1..4** (MUST/SHALL): `CHECKPOINT_FREQUENCY = 64`; `checkpointContainingLedger(L)`, `isLastLedgerInCheckpoint(L)`, `firstLedgerInCheckpointContaining(L)`, `sizeOfCheckpointContaining(L)`, ledger-zero pseudo-checkpoint at `history/00/00/00/history-00000000.json`.
- **Rust**: `crates/history/src/checkpoint.rs:22-274`, `crates/history/src/paths.rs:149-151`, `crates/history/src/lib.rs:689-760`.
- **Status**: Full. Doctests cover the boundary cases (`checkpoint_start(0..=128)`, `size_of_checkpoint_containing`).
- **Notes**: There's also `ACCELERATED_CHECKPOINT_FREQUENCY = 8` for `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`. Spec §16 calls 64 consensus-fixed; the test override is consistent with stellar-core's `getCheckpointFrequency`.

### §4.4 — HAS Structural Validation
- **Claim §4.4-1..8** (MUST): version range, networkPassphrase at v≥2, bucket level counts, bucket-version non-decreasing as level decreases, level-0 `next` clear, level-i `next` clear iff prev_snap version ≥ FIRST_PROTOCOL_SHADOWS_REMOVED=12 (else has resolved output hash), 100 GB bucket cap.
- **Rust**: `crates/history/src/verify.rs:709-761` (`verify_has_structure`), `crates/history/src/archive_state.rs:404-473` (`validate_bucket_list_structure`), `crates/history/src/archive_state.rs:23` (`MAX_HISTORY_ARCHIVE_BUCKET_SIZE`).
- **Status**: Full. Version monotonicity walks levels from deepest to 0 (matches spec's "snap before curr per level"), level-0 next check, version-12 branch in `validate_bucket_list_structure`.
- **Anchors**: `// Spec: CATCHUP_SPEC §3.1` at verify.rs:710, 727; `// Spec: CATCHUP_SPEC §4.4` at verify.rs:734.

### §4.5 — BucketList Hash
- **Claim §4.5-1**: Defined formula for v1 (live only) and v2 (`SHA256(liveHash || hotHash)`).
- **Rust**: `crates/history/src/archive_state.rs:600-654` (`compute_bucket_list_hash`).
- **Status**: Full. v2 path concatenates live + hot hashes correctly; v1 returns live only.

### §5 — Publishing Pipeline

- **§5.1 incremental build**: `crates/history/src/checkpoint_builder.rs:344-437` — three dirty streams (ledger / transactions / results), `.dirty` suffix, fsync-on-write via `XdrStreamWriter`. **Full**.
- **§5.2 finalization**: `checkpoint_builder.rs:444-481` — durable rename per file, parent dir create_dir_all. **Full**.
- **§5.3 HAS queue**: `crates/history/src/publish_queue.rs` (SQLite-backed; spec describes file-backed `<seq>.checkpoint.dirty` / `<seq>.checkpoint`). **Documented intentional implementation difference**: storage shape differs from spec's filesystem queue, but the crash-recovery semantics are equivalent. The queue is node-local (never externally observable), so no behavioral divergence exists at the consensus or interoperability level — however, the on-disk representation differs for local tooling and manual inspection. Henyey's queue entries are committed atomically in the ledger-close DB transaction (no pending/dirty row state); stellar-core achieves the same via `writeCheckpointFile` + `maybeCheckpointComplete` rename. Restart cleanup (`db.remove_publish_above_lcl(lcl)`) mirrors `restoreCheckpoint()`. See `publish_queue.rs` module docs for the full semantic mapping.
- **§5.4 upload + delete_published_files (max 100)**: `crates/history/src/publish.rs:71-124`. **Full**.
- **§5.5 diff-buckets**: `crates/history/src/archive_state.rs:220-274`. **Full** — order matches spec ("snap, next-output, curr per level, top→bottom"), inhibit set seeded with all-zero hash.
- **§5.6 backpressure 8/16**: `crates/history/src/publish_queue.rs:108-112`, `crates/history/src/catchup/replay.rs:559-585`. **Full**. Hysteresis verified at `catchup/replay.rs:568-572`; named tests `test_publish_queue_max_size_is_16`, `test_publish_queue_hysteresis_invariant` lock in the values.
- **§5.7 crash recovery**: `crates/history/src/checkpoint_builder.rs:491-680`. **Full**. Recovers dirty files by truncating entries with `ledgerSeq > lcl`, durably renames back. The §5.7 "all three or none" partial-state rule is enforced by category-by-category cleanup loop.

### §6.1 — Catchup Modes
- **Claim §6.1-1** (MUST): modes are OFFLINE_BASIC, OFFLINE_COMPLETE, ONLINE.
- **Rust**: `crates/history/src/catchup_range.rs` defines `CatchupRunMode::{OfflineBasic, OfflineComplete, Online}` as the spec §6.1 discriminator, and `CatchupConfiguration` wrapping both `CatchupMode` (replay depth/count) and `CatchupRunMode`. The discriminator is threaded through `catchup_to_ledger_with_config` and all app-level entry points.
- **Status**: Partial (structural only). The enum exists and is threaded through all entry points including the historywork/checkpoint-data fast path. `OFFLINE_COMPLETE` tx-result verification loop (§12) remains gated by #2831 — the mode value is carried but does not yet trigger the additional verification behavior.
- **Notes**: `CatchupMode::{Minimal, Complete, Recent(n)}` remains the replay-depth/count axis (spec `count` field). The two axes are intentionally separate.

### §6.3 — Range Computation
- **Claim §6.3-1..5** (numbered cases): five mutually exclusive cases.
- **Rust**: `crates/history/src/catchup_range.rs:221-319`.
- **Status**: Partial. Henyey adds an extra Case 0 ("Complete from genesis → full replay") and Case 4b for completeness. The spec's Case 1 ("LCL > genesis → replay") is at line 259. Cases 2/3 are at lines 268-286. Case 4 (target start in first checkpoint) at 290-296.
- **Drift item**: spec §6.3 Case 1 says "Replay forward from LCL+1" unconditionally when `lcl > init`. Rust §6.3 collapses this with the post-#2677 invariant: bucket-apply only when LCL == genesis. This is documented in code (`catchup_range.rs:253-262`) and matches stellar-core CatchupRange.cpp:52-57 exactly. **Not a true drift — matches stellar-core; spec phrasing is just one of multiple valid factorings of the same five cases.**

### §8 — Catchup Pipeline
- **§8.1 fetch HAS**: `catchup/mod.rs:578-616` (`download_and_verify_has`), `:643-712` (`catchup_to_ledger`). **Full**.
- **§8.2 download + verify**: `catchup/mod.rs:540-545` (`download_verify_and_replay_with_retry`), `verify.rs:253-441` (`verify_reverse_walk`). **Full**.
- **§8.3 herder state restore**: not located in this crate. **Marked N/A** — herder/state coordination is in `crates/herder` / `crates/app`.
- **§8.4 apply buffered**: lives in `crates/app/src/app/ledger_close.rs:1758-1792`. **N/A**.
- **§8.5 post-apply asserts**: `catchup/mod.rs:463-486`. **Full**. Both (a) HAS/header seq agreement and (b) INV-C15 (`checkpoint_header.ledger_seq >= lcl_seq`).

### §9 — Ledger Chain Verification
- **§9.1 trust anchors**: `verify.rs:53-67` defines `TrustSource::{Scp{seq,hash}, None}` and `ChainTrustAnchors`. Online catchup now derives the trust anchor from `Herder::check_ledger_close(target+1)` via `App::resolve_scp_trust_anchor()` and passes it through `CatchupManager::set_trusted_scp_anchor()` to the reverse-walk config in `catchup/replay.rs`. **Full**. Matches stellar-core's `LedgerApplyManagerImpl::startOnlineCatchup()` → `VerifyLedgerChainWork` path. Offline/synthetic callers without an adjacent externalized slot still get `TrustSource::None`.
- **§9.2 reverse-walk algorithm**: `verify.rs:253-441`. **Full**. Partitions into checkpoint groups, walks reverse, threads cross-checkpoint link, checks `previous_ledger_hash` continuity, LCL+1 link check (verify.rs:393-405).
- **§9.3 outcome → fatal**: `verify.rs:422-436`. **Full**. When `local_state_disagrees && TrustSource::Scp`, returns `FatalChainDisagreement`; with `TrustSource::None`, returns `InvalidPreviousHash` (retryable). Matches §9.3 table.

### §10 — Bucket Application
- **§10.1 download + verify**: bucket size cap at `catchup/download.rs:230-240`; per-bucket hash check at `catchup/buckets.rs:381-405`. **Full**.
- **§10.2 `containsValidBuckets`**: `archive_state.rs:749-790`, called at `catchup/buckets.rs:447-456` post-restore. **Full** (§10.2 says "before any application"; Rust runs it inside `apply_buckets` post-restore but before DB mutation — observable behavior identical).
- **§10.3 apply algorithm + newest-wins**: cross-crate (`crates/bucket/src/bucket_list.rs:1969-1989` + `manager.rs:850-890`). **N/A** for INV-C9 (see invariant table).
- **§10.4 index buckets**: indirect via `BucketList::restore_from_has` in `crates/bucket`. **Full**.
- **§10.5 AssumeState**: `catchup/buckets.rs:154-216` (`restart_merges`). **Full**.
- **§10.6 post-apply LCL setup + REBUILD_FOR_OFFER_TABLE clearing**: **Full** — `catchup/mod.rs:500-502` calls `ledger_manager.reset()` + `initialize()`. The literal `REBUILD_FOR_OFFER_TABLE` flag is not needed because henyey has no SQL offer table; the equivalent recovery contract is provided by the two-window design documented in §14.5 below and `crates/app/README.md`. Note: §14.5 itself is Partial due to CLI reader paths; the `reset()` + `initialize()` sequence here is fully equivalent to stellar-core's post-apply LCL setup.

### §11 — Transaction Replay
- **§11.1 per-checkpoint workflow**: `catchup/replay.rs:460-552`. **Full**. After each apply: hash mismatch yields `ReplayHashMismatch`.
- **§11.2 five-case knit-to-LCL**: `catchup/replay.rs:58-117`. **Full**. Order of checks matches stellar-core's `ApplyCheckpointWork.cpp:246` (case 5 before case 4). Tests `test_knit_case_1..5` cover all branches.
- **§11.3 tx-set hash check**: `verify.rs:596-631` (`verify_tx_set`), called at `catchup/replay.rs:248-249`. **Full**. Mismatch produces `TxSetHashMismatch` with format diagnostic.
- **§11.4 backpressure**: `catchup/replay.rs:480-483, 559-585`. **Full**. Gated by `replay_config.wait_for_publish`.
- **§11.5 gaps**: tx_set is always available (synthesized for absent entries via `LedgerData::tx_set()`). Matches stellar-core. **Full**.

### §12 — Transaction Results Verification (OFFLINE_COMPLETE)
- **Claim §12-1..4** (MUST in OFFLINE_COMPLETE, SHALL NOT in OFFLINE_BASIC/ONLINE): for every non-genesis ledger, verify `sha256(xdr_to_opaque(L.txResultSet)) == L.header.txSetResultHash`; genesis ledger exempt iff results empty.
- **Rust**: `verify.rs:542-563` (`verify_tx_result_set`) and `verify.rs:808-842` (`verify_tx_result_ordering`) exist.
- **Status**: Partial. The verification *function* is correct and tested (`test_verify_tx_result_set_genesis_empty/nonempty/non_genesis_empty`), but it is called from `replay/execution.rs:680` during live replay — not from a separate `VerifyTxResultsWork`-equivalent that runs over the entire replay range in OFFLINE_COMPLETE mode. Henyey has no OFFLINE_COMPLETE entry point at all (see §6.1). The MUST-NOT clauses for OFFLINE_BASIC/ONLINE are trivially satisfied (no caller invokes the function in those modes), but the MUST clause for OFFLINE_COMPLETE has no caller because the mode does not exist as a configuration option.

### §14.5 — Catchup-side crash recovery
- **Claim §14.5-1**: A crash during bucket apply must not leave the node in an inconsistent state; on restart, the node must detect the interruption and force safe recovery (re-catchup).
- **stellar-core mechanism**: `REBUILD_FOR_OFFER_TABLE` flag set before bucket apply (SQL offer tables are mutated incrementally), cleared after `setLastClosedLedger()`; startup `maybeRebuildLedger()` detects and rebuilds.
- **henyey mechanism (two-window design)**:
  - **Window 1 (pre-final-persist):** `persist_bucket_list_snapshot()` (`catchup/mod.rs:488`), `persist_header_only()` (`catchup/mod.rs:526`), `persist_ledger_history()` (`catchup/replay.rs`), and `emit_meta()` write checkpoint-keyed or per-ledger rows that are durable but non-authoritative for the startup restore path. Startup reads from `last_closed_ledger` / `HISTORY_ARCHIVE_STATE` (in `crates/app`), not from ahead-of-LCL rows. A crash here leaves orphaned rows overwritten by the next successful catchup. **Intra-batch ordering (#3811):** within the per-checkpoint streaming replay loop, `persist_ledger_history()` runs **before** `replay_via_close_ledger()` advances the in-memory LCL, so the durable history rows can only ever *lead* the LCL, never lag it. This is the direction stellar-core enforces: `getHistoryManager().appendTransactionSet` + `storePersistentStateAndLedgerHeaderInDB` (`LedgerManagerImpl.cpp:1661`, `:2963-2966` via `:3122-3129`) precede `ltx.commit()` (`:1835`), which precedes the in-memory advance `advanceLastClosedLedgerState` (`:1409`, dispatched `:1878-1894`). The one-sided invariant "persisted history record set ⊇ closed-ledger set" — ahead-of-LCL is truncated at restart, behind-LCL is corruption core refuses to boot from — is stated by `CheckpointBuilder::cleanup(lcl)` with `enforceLCL = true` for the ledger-header file (`CheckpointBuilder.cpp:199-345`). Before #3811 the reorder was inverted, so a persist failure after replay had already advanced LCL could resume past an unpersisted batch and report success with a ≤`checkpoint_frequency()` (64) ledger `ledgerheaders`/`txhistory`/`txresults` hole. **Caveats:** (1) `verify_on_disk_integrity()` (`crates/app/src/app/mod.rs:1406`) also starts from `get_latest_ledger_seq()` and runs before `check_catchup_persist_pending()` on startup; after a Window-1 crash it walks ahead-of-LCL headers, but because catchup writes headers sequentially the hash chain stays valid — harmless over-verification. (2) CLI readers (`publish_history`, `self_check`) still use `get_latest_ledger_seq()` → `MAX(ledgerseq)` and may observe ahead-of-LCL state; this is a known gap. (3) henyey has no `cleanup(lcl)`-equivalent sweep to truncate ahead-of-LCL history rows on restart (tracked in #3812).
  - **Window 2 (post-catchup / pre-deferred-persist):** `crates/app/src/app/catchup_impl.rs` sets `CATCHUP_PERSIST_PENDING` before handing off deferred persist. `App::new()` → `check_catchup_persist_pending()` detects it on restart and seeds `catchup_needs_full_reset`, forcing full bucket-apply recovery. The sentinel is cleared atomically with the final state write in `CatchupPersistData::write_to_db` (`crates/app/src/app/persist.rs`).
- **Status**: **Partial** (startup/catchup recovery path has semantic parity; `verify_on_disk_integrity()` is safe — hash chain stays valid for orphaned headers — but CLI reader paths still anchor on `MAX(ledgerseq)` instead of durable LCL/HAS). The core recovery contract — startup never trusts interrupted catchup state — is preserved without a literal `REBUILD_FOR_OFFER_TABLE` flag because henyey has no SQL offer table (BucketListDB-only with in-memory offer index; see `crates/ledger/PARITY_STATUS.md`). Full parity requires hardening CLI readers to use LCL/HAS as their authority anchor.

### §16 — Constants
All constants present with correct values:
- `CHECKPOINT_FREQUENCY = 64` at `checkpoint.rs:22` (with test override `ACCELERATED_CHECKPOINT_FREQUENCY = 8`).
- `MAX_HISTORY_ARCHIVE_BUCKET_SIZE = 100 * 1024^3` at `archive_state.rs:23`.
- `FIRST_PROTOCOL_SHADOWS_REMOVED = 12` at `archive_state.rs:378`.
- `GENESIS_LEDGER_SEQ = 1` at `catchup_range.rs:24`.
- `PUBLISH_QUEUE_MAX_SIZE = 16`, `PUBLISH_QUEUE_UNBLOCK_APPLICATION = 8` at `publish_queue.rs:108-112`.
- `MAX_DELETE_CHECKPOINTS = 100` at `publish.rs:71` (named `MAX_PUBLISH_DELETE_CHECKPOINTS` in spec — naming drift, value matches).
- `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION = 23`: not located as a named constant; logic at `publish.rs:295` (`version = if hot_archive_buckets.is_some() { 2 } else { 1 }`) is the structural equivalent. **Naming drift**, semantic Full.
- `LIVE_BUCKETLIST_LEVELS = 11`: re-exported from `henyey_bucket::BUCKET_LIST_LEVELS`; same value.
- `HOT_ARCHIVE_BUCKETLIST_LEVELS`: re-exported from `henyey_bucket::HOT_ARCHIVE_BUCKET_LIST_LEVELS`.
- `MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT = 12`: not located in this crate. Lives in `crates/app` if at all — N/A here.
- `CONDITIONAL_APPLY_POLL_INTERVAL = 500 ms`: not located. Replay loop in `catchup/replay.rs:583` sleeps 1 second between backpressure polls (drift in interval); the spec §11.1 pre-apply merge wait at 500 ms is N/A here (lives in caller / `crates/bucket`).

---

## Invariant Coverage

| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-C1 (Chain monotonic) | Full | `verify.rs:312-332` (forward link within group), `verify.rs:515-530` (`verify_ledger_header_history_entry` hashes header). |
| INV-C2 (Checkpoint alignment) | Full | `verify.rs:282-292` (group partition by checkpoint); checkpoint ledger range enforced via `verify_tx_result_ordering` (`verify.rs:808-842`). |
| INV-C3 (HAS integrity) | Full | `verify.rs:709-761` + `archive_state.rs:404-473`. |
| INV-C4 (BucketList hash agreement) | Full | `replay/execution.rs:140-200` (`verify_bucket_list_hash`, called under `config.verify_bucket_list`) + `archive_state.rs:600-654` (`compute_bucket_list_hash`). Verified at checkpoint ledgers (and ≥V23 only when eviction ran), matching stellar-core's checkpoint-boundary assertion in `CatchupWork.cpp:259-260`. |
| INV-C5 (Trust anchor authentication) | Full | Verification function supports `TrustSource::Scp` (`verify.rs:355-384`) and online catchup now plumbs the SCP-derived anchor from `App::resolve_scp_trust_anchor()` → `CatchupManager::set_trusted_scp_anchor()` → `catchup/replay.rs`. LCL disagreement is fatal when trusted hash exists. |
| INV-C6 (Tx result hash check) | Partial | `verify_tx_result_set` correct (`verify.rs:542-563`) but no `OFFLINE_COMPLETE`-mode caller that loops over every replay-range ledger. See §12. |
| INV-C7 (Knit-to-LCL exclusivity) | Full | `catchup/replay.rs:58-117` — five mutually-exclusive cases; case 5 returns `KnitOvershot` error (fatal). |
| INV-C8 (Buffered drain ordering) | N/A | Lives in `crates/app/src/app/ledger_close.rs:1758-1792` (`try_apply_buffered_ledgers` only advances `next_seq = current + 1`; gap stops the chain). |
| INV-C9 (Bucket-apply newest wins) | N/A | Lives in `crates/bucket/src/bucket_list.rs:1969-1989` and `crates/bucket/src/manager.rs:850-890` (`seen_keys` HashSet, shallowest-first traversal). |
| INV-C10 (Publish file finalization boundary) | Full | `checkpoint_builder.rs:444-481` rename happens only after caller passes the checkpoint ledger; `recover_dirty_file` (`:135-225`) truncates `ledgerSeq > lcl` on restart. |
| INV-C11 (Publish queue durability) | Full | `publish_queue.rs` (SQLite-backed; row-level atomic); on restart, queue rows survive iff their transaction committed. Storage shape differs from spec's filename-based scheme but provides equivalent crash-recovery semantics (documented intentional difference — see §5.3 drift item). |
| INV-C12 (No retry on fatal) | N/A | Lives in `crates/app/src/app/mod.rs:557` (`fatal_state_failure: AtomicBool`); checked at `crates/app/src/app/ledger_close.rs:1801` and `crates/app/src/app/catchup_impl.rs:55,1642`. |
| INV-C13 (Range exclusivity) | Full | `catchup_range.rs:155-208` enum encodes the three cases (`ReplayOnly` / `BucketApplyAndReplay` / `BucketsOnly`) at the type level; `buckets_and_replay` enforces `checkpoint + 1 == replay.first` via assert (line 202-206). |
| INV-C14 (Replay determinism) | Full | Replay always goes through `ledger_manager.close_ledger()` (`catchup/replay.rs:516-531`), the same path as live close, with `expected_header_hash` propagated and verified (`catchup/replay.rs:489-501, 519-525`). Determinism property held by `crates/ledger`. |
| INV-C15 (Catchup never applies older state) | Full | `catchup/mod.rs:473-486` explicitly checks `checkpoint_header.ledger_seq >= lcl_seq` before any DB mutation; produces `VerificationFailed` with the INV-C15 label. Additionally guarded at the type level by `catchup_range.rs:253-262` (bucket-apply only on LCL == genesis path). |

---

## Existing Spec Anchors (11 total)

| Location | Section |
|----------|---------|
| `archive_state.rs` (3 anchors via doc comments) | §3.1, §4.4 |
| `checkpoint.rs:164, 255, 388, 444` | §4.3 |
| `verify.rs:710, 727, 734` | §3.1, §4.4 |
| `catchup/mod.rs:463` | §8.5 |
| `catchup/replay.rs:472, 556` | §5.6, §11.4 |
| `historywork/builder.rs:136, 147, 158` | §9.1 (retry policy) |
| `publish_queue.rs:541, 547` | §5.6 |

No dangling anchors were detected — all cited sections exist in the regenerated v26.0.1 spec.

---

## Drift Items (human review)

1. **§5.3 HAS publish queue storage shape** *(Documented intentional difference)*: spec describes a filesystem of `<seq>.checkpoint` and `<seq>.checkpoint.dirty` files; Rust uses a SQLite `publishqueue` table (`publish_queue.rs`). The durability semantics are equivalent for crash-recovery purposes (transactional commit replaces durable rename), and the queue is node-local (never exposed in archives or externally observable). However, the on-disk storage shape still differs from the spec's filesystem model, which matters for local operational tooling, debugging, and manual inspection. **Documented in-repo** as an intentional implementation difference — see `publish_queue.rs` module docs, `README.md` design note, and `PARITY_STATUS.md` architectural difference #3. No code change required.

2. **§6.3 Case 1 / post-#2677 invariant**: Rust collapses spec's five-case range computation into a slightly different factoring (Case 0 for Complete-from-genesis, Case 1 unconditional replay when LCL > genesis, Case 4b for first-checkpoint targets). The factoring is documented at `catchup_range.rs:240-262` and stated to match stellar-core CatchupRange.cpp:52-57 exactly. **Not a true drift — request a spec-side update to make the factoring explicit, citing PR #2677.**

---

## Recommendations

1. **~~Add OFFLINE_BASIC / OFFLINE_COMPLETE / ONLINE mode discriminator~~** ✅ Done (#2829). Wire OFFLINE_COMPLETE to a `verify_results_for_range` work that loops over `results-*.xdr.gz` files for every checkpoint in the replay range, invoking `verify_tx_result_set` per ledger. Closes the §12 / INV-C6 gap (#2831).
2. ~~**Plumb SCP-side trusted hash through** to `catchup/replay.rs:235` so `TrustSource::Scp` is actually exercised in ONLINE mode. Closes the INV-C5 gap (currently relies on internal-only chain consistency).~~ *(Done — #2830. `App::resolve_scp_trust_anchor()` derives the anchor from `Herder::check_ledger_close(target+1)` and passes it to `CatchupManager::set_trusted_scp_anchor()`.)*
3. ~~**Add a `REBUILD_FOR_OFFER_TABLE`-equivalent persistent-state flag**~~ *(Partially resolved — §14.5 two-window design covers the startup/catchup recovery path. Remaining gap: CLI readers (`publish_history`, `self_check`) anchor on `MAX(ledgerseq)` instead of durable LCL/HAS and may observe ahead-of-LCL state after a Window-1 crash. Needs hardening.)*
4. ~~**Update spec §5.3** to acknowledge SQLite-backed publish queue as a conforming alternative storage shape (or vice versa, if filesystem-backed is required for stellar-core interoperability).~~ *(No spec update needed — documented in-repo as intentional implementation difference; queue is node-local. Tracked as Drift 1 above.)*
5. **Update spec §6.3** to make the post-#2677 collapsed factoring of Case 1 / Case 2 / Case 4b explicit, eliminating the apparent drift.
