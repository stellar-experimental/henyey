# BUCKETLISTDB_SPEC Adherence — henyey-bucket

**Spec version:** 26 (stellar-core v26.0.1 / Protocol 26)
**Crate:** crates/bucket
**Last updated:** 2026-05-13
**Overall adherence:** 86%

**Counts:** Full 56 | Partial 8 | Absent 1 | Drift 2 | N/A 5

> Henyey targets Protocol 24+ exclusively. Pre-P24 constructs (pre-P11 LIVEENTRY
> semantics, P11/P12-only shadow algorithms, V0 bucket metadata extensions) are
> classified as N/A or Partial-with-P24+-waiver. Determinism within the P24+
> scope is enforced; the P24+ waiver is acknowledged at the spec anchor and in
> the `MergeKey` design note (`future_bucket.rs:72-79`).

---

## Summary Table

| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §3.1 | BucketEntry tagged union | Full | entry.rs (re-export) |
| §3.2 | HotArchiveBucketEntry validation | Full | hot_archive.rs:184-200 |
| §3.3 | Sort order — metadata first, LedgerKey | Full | entry.rs:174-181 |
| §3.4 | BucketEntryCounters | Full | index.rs:83 |
| §4.1 | `kNumLevels = 11` | Full | bucket_list.rs:79 (`BUCKET_LIST_LEVELS`) |
| §4.2 | `levelSize`, `levelHalf` | Full | bucket_list.rs:99-107, eviction.rs:430-437 |
| §4.3 | Spill condition | Full | bucket_list.rs:119-128 |
| §4.4 | Update period | Full | eviction.rs:469-481 |
| §4.5 | Tombstone retention | Full | bucket_list.rs:133-135 |
| §4.6 | Oldest ledger tracking | Absent (informational) | not observable; consensus-irrelevant |
| §4.7 | BucketList hash (live levels) | Full | bucket_list.rs:1629-1637, 775-784 |
| §4.7 | `bucketListHash` composition (live‖hotArchive) | Full | history/replay/execution.rs:468 (out-of-crate) |
| §4.8 | LedgerHeader skip list | Full | ledger/header.rs:104-124 (out-of-crate) |
| §5.1 | Entry conversion | Full | bucket_list.rs:2112-2197 |
| §5.2 | `fresh` creation | Full | bucket.rs (Bucket::from_entries / fresh_in_memory_only); hot_archive.rs:177-239 |
| §5.3 | `addBatch` per-ledger sequence | Full | bucket_list.rs:2221-2403 |
| §5.4 | `prepare` / `shouldMergeWithEmptyCurr` | Full | bucket_list.rs:145-164, 983-1100 |
| §5.5 | Level-0 in-memory merge | Full | merge.rs:589-732; bucket_list.rs:1203-1265 |
| §5.6 | snap and commit | Full | bucket_list.rs:809-846, 968-981 |
| §6.1 | Effective merge protocol | Partial | merge.rs:1071-1102 (P24+ shadow waiver, see drift) |
| §6.2 | Merge loop | Full | merge.rs:164-286 |
| §6.3 | Shadow elision | Partial | merge.rs:308-326, 803-821 (pre-P12 path retained but unreachable under P24+) |
| §6.4 | Equal-key merge rules (Live) | Full | merge.rs:864-926 |
| §6.5 | Equal-key merge rules (HotArchive) | Full | hot_archive.rs:1860-1901 |
| §6.6 | Tombstone elision at deepest level | Full | merge.rs:826-831, iterator.rs:447-480 |
| §6.7 | In-memory merge | Full | merge.rs:589-732 |
| §6.8 | Output bucket identity | Full | manager.rs:287-310; merge.rs:417-436 |
| §7.1 | FutureBucket state machine | Full | future_bucket.rs:279-308, 434-442 |
| §7.2 | Construction & merge start | Partial | future_bucket.rs:332-378 (no explicit `snap.ver>=V12 && shadows` reject — unreachable under P24+) |
| §7.3 | Merge deduplication via MergeKey | Partial | future_bucket.rs:72-99, merge_map.rs:251-271 (MergeKey omits `shadowHashes`; P24+ waiver) |
| §7.4 | Resolution | Full | future_bucket.rs:521-620 |
| §7.5 | makeLive | Full | future_bucket.rs:695-762 |
| §8.1 | Adoption | Partial | manager.rs:287-310, 778-798 (no explicit `noteEmptyMergeOutput`; empty merges short-circuit) |
| §8.2 | Garbage collection | Partial | manager.rs:706-759, merge_map.rs:326-339 (set-based; use_count-based path replaced) |
| §8.3 | Statistics | Full | bucket_list.rs:1697-1709, index.rs (BucketEntryCounters) |
| §9.1 | Lookup semantics | Full | index.rs, snapshot.rs:372-406 |
| §9.2 | InMemoryIndex | Full | index.rs:316-510 |
| §9.3 | DiskIndex | Full | index.rs:523-740; bloom_filter.rs:68-130 |
| §9.4 | Type range map | Full | index.rs (TypeRange) |
| §9.5 | AssetPoolIDMap | Full | index.rs:207-280, 469, 698 |
| §9.6 | Entry cache | Full | cache.rs (BucketEntryCache); bucket_list.rs:1716-1733 |
| §9.7 | Persistence + version invalidation | Full | index_persistence.rs:53, 427-456 (`BUCKET_INDEX_VERSION = 5`) |
| §10.1 | BucketSnapshotManager | Full | snapshot.rs:1152-1276 |
| §10.2 | Point lookup | Full | snapshot.rs:372-406 |
| §10.3 | Bulk load | Full | snapshot.rs:416-500 |
| §10.4 | Pool share trust lines | Full | snapshot.rs:961-986 |
| §10.5 | Inflation winners (legacy) | Full | snapshot.rs:871-937 |
| §10.6 | Entry type scan | Full | snapshot.rs:804-851 |
| §11.1 | Hot Archive purpose | Full | hot_archive.rs (entire) |
| §11.2 | Hot Archive structure | Full | hot_archive.rs:1031-1139 |
| §11.3 | Hot Archive entry types & sort | Full | hot_archive.rs:1700-1768 |
| §11.4 | Hot Archive merge rules | Full | hot_archive.rs:1773-1910 |
| §12.1 | Eviction iterator | Full | eviction.rs:34-241 |
| §12.2 | Starting position | Full | eviction.rs:489-541 |
| §12.3 | Scan process | Full | eviction.rs:553-660; snapshot.rs:512-580 |
| §12.4 | In-bucket scan + newest-version replacement | Partial | eviction.rs:553-660 (unconditional newest-version replacement; P24+ waiver, drift vs P23 bug) |
| §12.5 | Validity check | Absent | no explicit `EvictionResultCandidates::isValid` analogue |
| §12.6 | Resolve eviction scan / EvictedStateVectors | Full | eviction.rs:282-396 |
| §13.1 | Bucket application | Partial | applicator.rs:297-367 (intentional model divergence; OFFER restriction removed, treats LIVE = INIT uniformly) |
| §13.2 | Application order | Full | applicator.rs (driven by chunk_size & per-bucket iteration) |
| §13.3 | State reconstruction (assumeState) | Full | bucket_list.rs:2605-3145 (`restore_from_hashes`, `restart_merges_from_has`) |
| §14.1 | HAS round-trip + 100 GiB cap | Full | history/archive_state.rs:23 (`MAX_HISTORY_ARCHIVE_BUCKET_SIZE`, out-of-crate) |
| §14.2 | Bucket directory layout | Full | manager.rs:canonical_bucket_filename |
| §14.3 | Checkpoint alignment | N/A | publishing pipeline in `henyey-history` |

---

## Invariant Coverage

| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-B1 — Deterministic BucketList hash | Full | bucket_list.rs:1629-1637; merge.rs:1071-1102 |
| INV-B2 — Monotonic Level 0 update | Full | bucket_list.rs:2349-2351 (`prepare_first_level` + commit per ledger) |
| INV-B3 — Spill schedule | Full | bucket_list.rs:119-128, 2248-2343 (descending iteration) |
| INV-B4 — Effective merge protocol | Partial | merge.rs:1071-1102 — shadow-version max omitted (P24+ waiver; documented at merge.rs:1066-1070) |
| INV-B5 — Shadow elision pre/post INITENTRY | Partial | merge.rs:308-326, 803-821 — P11/<12 branch retained but unreachable under P24+ scope |
| INV-B6 — INIT/DEAD annihilation | Full | merge.rs:864-926 — full table match including `panic!("Malformed bucket: old non-DEAD + new INIT")` |
| INV-B7 — Eviction partitioning | Full | eviction.rs:308-396 (`ResolvedEviction { archived_entries, deleted_keys }`) |
| INV-B8 — Tombstone elision only at deepest level | Full | merge.rs:826-831; bucket_list.rs:133-135 (`keep_tombstone_entries`) |
| INV-B9 — Bucket immutability | Partial | manager.rs:287-310 — content-addressed by hash; explicit "two buckets MUST NOT exist with the same hash but distinct files" check is not present (hashing is deterministic so the invariant holds by construction) |
| INV-B10 — FutureBucket state invariants | Full | future_bucket.rs:279-308 — state-validity is structurally enforced via enum variants (invalid states unrepresentable) |
| INV-B11 — Merge identity | Partial | merge_map.rs:251-271; future_bucket.rs:81-99 — MergeKey scope-narrowed to `(keep_tombstones, curr_hash, snap_hash)`; correct under P24+ |
| INV-B12 — Last-level INIT correctness during apply | Drift | applicator.rs:319-340 — see drift item below (intentional model divergence) |
| INV-B13 — Hot Archive content constraint | Full | hot_archive.rs:184-200 (fresh validates `is_persistent_entry`/`is_persistent_key`) |
| INV-B14 — HAS round-trip | Full | bucket_list.rs:2605-3145; tests cover restore_from_hashes / restart_merges round-trip |
| INV-B15 — `bucketListHash` composition | Full | crates/history/src/replay/execution.rs:468 (`combined_bucket_list_hash`, P23+ branch) |
| INV-B16 — Metadata first | Full | entry.rs:216-266 (`StreamingSortedValidator` rejects duplicate or trailing meta) |

---

## Detailed Findings (by section)

### §3.3 Sort Order
`compare_entries` (`entry.rs:174-181`) treats `Metaentry` (no key) as strictly
less than every keyed entry; `compare_keys` (`entry.rs:150-152`) delegates to
`LedgerKey`'s derived `Ord`, which matches stellar-core's xdrpp recursive
field-by-field comparator (rs-stellar-xdr declares enum variants and struct
fields in XDR declaration order). The streaming validator
(`entry.rs:216-266`, INV-B16) enforces:
- at most one metadata entry,
- metadata-before-keys,
- strict-ascending keyed sequence.

### §4.7 BucketList Hash
`BucketList::hash()` at `bucket_list.rs:1629-1637` concatenates
`level.hash()` for each level and SHA-256s the result; per-level
`level.hash()` at `bucket_list.rs:775-784` does
`SHA256(curr.hash() || snap.hash())`. Matches `§4.7` exactly.

The combined `bucketListHash` for protocol ≥23 (`SHA256(live || hot_archive)`)
lives in `crates/history/src/replay/execution.rs:468` (`combined_bucket_list_hash`).
Although primary scope is the `bucket` crate, this is the canonical
implementation of INV-B15 in the workspace.

### §5.3 addBatch
`add_batch_internal` at `bucket_list.rs:2221-2403` processes spills in
descending order from `kNumLevels-1` to 1, calls `commit()` then
`prepare_with_normalization()` on each spilling level, then applies the
new batch to level 0 via `prepare_first_level()`. Shadow gathering at
line 2303-2313 falls through to empty for protocols >= V12 (P24+ never
populates shadows). The spec's "pop last two shadows" step is implicitly
captured because under P24+ the shadow vector is unconditionally empty.

### §5.5 Level 0 In-Memory Merge
`prepare_first_level` (`bucket_list.rs:1203-1265`) selects the in-memory
fast-path when both inputs have in-memory entries, otherwise falls back
to the on-disk merge path. `merge_in_memory` (`merge.rs:589-732`) keeps
the result in memory for the next ledger and persists it to disk on the
next `add_batch_internal` background-persist pass (`bucket_list.rs:2362-2401`).
The metadata version is set to `max_protocol_version` directly
(matches stellar-core `LiveBucket::mergeInMemory` `LiveBucket.cpp:569`).

### §6.4 Equal-Key Merge Rules (Live) — heart of bucket correctness
The full normative matrix from `§6.4` is implemented at `merge.rs:864-926`:

| old → new | INIT = y | LIVE = y | DEAD |
|-----------|----------|----------|------|
| INIT = x | panic!() at line 901-903 | `Initentry(y)` at 884-886 | `None` (annihilation) at 874 |
| LIVE = x | panic!() at line 901-903 | `Liveentry(y)` at 889-891 | `Deadentry(key)` at 906-912 if keep else None |
| DEAD    | `Liveentry(x)` at 878-880 | `Liveentry(y)` at 894-896 | `Deadentry(key)` at 915-921 if keep else None |

Specifically:
- `(INIT, INIT)` and `(LIVE, INIT)` are routed through the wildcard arm at
  line 901-903 and panic with the canonical message
  `"Malformed bucket: old non-DEAD + new INIT."` — matches stellar-core
  `LiveBucket::mergeCasesWithEqualKeys`.
- `(DEAD, INIT=x) → LIVE=x` per the second arm (spec: emit `LIVE = x`).
- `(INIT=x, LIVE=y) → INIT=y` per the third arm.
- `(INIT, DEAD) → None` (annihilation) per the first arm.
- `(LIVE/DEAD, DEAD)` writes a tombstone iff `keep_dead_entries == Keep`,
  matching `§6.6` interaction.

The four "easy" cases (old<new, new<old, both-exhausted, equal) are
implemented at `merge.rs:236-282` via key-compare on the peeked head
of each iterator.

### §6.6 Tombstone Elision at the Deepest Level
`should_keep_entry` at `merge.rs:826-831` drops `Deadentry` when policy
is `Remove`; `BucketList::keep_tombstone_entries` at `bucket_list.rs:2473-2479`
sets `Remove` exclusively when `level == kNumLevels - 1`. Hot Archive
analog at `hot_archive.rs:1865, 1871, 1880, 1886, 1894` honors
`keep_tombstones` independently.

Note: independent of the keep-dead policy, `BucketOutputIterator::put`
at `iterator.rs:451-454` also drops dead entries when its
`keep_tombstones` flag is false, providing a second-layer defense
that mirrors stellar-core's `BucketOutputIterator::maybePut`.

### §7.1 FutureBucket State Machine
The state diagram (Clear, HashOutput, HashInputs, LiveOutput, LiveInputs)
is implemented at `future_bucket.rs:279-308` as a Rust enum
(`FutureBucketInner`) where each variant carries exactly the fields it
needs — making the spec's "invalid" combinations unrepresentable. The
`check_state()` method at line 444-473 is retained for API compatibility
but is now a no-op (deprecated, asserts only hash/bucket consistency
in debug builds).

### §7.3 MergeKey
`MergeKey` at `future_bucket.rs:81-99` is `(keep_tombstones, curr_hash, snap_hash)`.
The spec's `MergeKey` is `(keepTombstoneEntries, currHash, snapHash, shadowHashes)`.
Henyey omits `shadowHashes` because under P24+ shadows are always empty
(shadows were removed at protocol 12). The omission is documented inline
at `future_bucket.rs:72-79`. Correct within henyey's stated scope.

### §10.5 Inflation Winners
`load_inflation_winners` at `snapshot.rs:871-937`:
- per-account `balance >= 1_000_000_000` to contribute votes (line 904) ✓
- accumulated `votes >= min_balance` to qualify as winner (line 926) ✓
- sorted by votes descending, truncated to `max_winners` (lines 931-934) ✓
- youngest-level wins via `seen_accounts` (lines 893-896) ✓
- `Deadentry::Account` marks the account as seen but contributes no votes (line 912-915) ✓

The doc comment at `snapshot.rs:898-902` cites `§10.6` but the matter is
actually specified in `§10.5` ("Inflation Winners (Legacy)"). See
Dangling Anchors below.

### §12.4 In-Bucket Scan / Newest-Version Replacement
`scan_bucket_region` (`eviction.rs:553-660`) unconditionally replaces the
candidate payload with the newest version returned by the bulk-load
(`eviction.rs:626-640`). The spec's `§12.4` makes this behavior
@version(≥24) only — at @version(=23) the "older payload" bug is
required for determinism. Documented inline (`eviction.rs:621-624`).
Under henyey's P24+ scope this is correct; if pre-P24 catchup were
introduced, a version guard would be required. Flagged as **Partial** /
P24+ waiver, not Drift.

### §13.1 Bucket Application — Intentional Model Divergence
`BucketApplicator::advance` (`applicator.rs:297-367`) treats `LIVE` and
`INIT` entries identically (both become `Upsert`) and does **not**
restrict to OFFER-range entries. This is a deliberate divergence
documented at `applicator.rs:319-327`: stellar-core distinguishes
deepest-level `LIVE` (treat as `create` per INV-B12) and restricts
non-BucketListDB apply to the OFFER range. Henyey operates exclusively
in BucketListDB mode where:
1. all entry types are served directly from the BucketList query layer
   (`§10`), and
2. the bucket-applicator's catchup path is an idempotent
   upsert/delete sequence — `seen_keys` ensures youngest-wins semantics
   so the LIVE-vs-INIT distinction at the deepest level is unobservable.

Classified as **Drift (INV-B12)** for transparency; behavior is
functionally equivalent under henyey's invariants. See Drift Items
below.

---

## Dangling Spec Anchors

- `crates/bucket/src/snapshot.rs:900` cites `BUCKETLISTDB_SPEC §10.6`
  for the 1B-stroop minimum balance rule in `load_inflation_winners`.
  The rule is in **§10.5 Inflation Winners (Legacy)**; §10.6 is
  "Entry Type Scan". Fix: change `§10.6` → `§10.5`.
- `crates/bucket/src/snapshot.rs:1718` (test comment) repeats the same
  off-by-one. Fix: change `§10.6` → `§10.5`.

All other in-crate anchors (§3.4, §6.3, §6.4, §6.6, §7.1, §7.3, §8.2,
§11.4, §12, §12.4, §13.1) resolve to the correct (existing) sections in
the regenerated v26 spec.

---

## Drift Items (require human review)

- **§13.1 / INV-B12 (Last-level INIT correctness).** Henyey's BucketListDB
  applicator (`applicator.rs:297-367`) does **not** promote a deepest-level
  `LIVEENTRY` to `INITENTRY` semantics during catchup apply (the spec
  requires "MUST be treated as INITENTRY (a create)"). Under henyey's
  invariants — all entry types served from the BucketList, idempotent
  upsert/delete, youngest-wins via `seen_keys` — the externally observable
  state is identical, because:
  1. The applicator's `EntryToApply::Upsert` is equivalent to both
     `create` and `update` at the storage layer (no FK or
     create-vs-update predicate).
  2. The youngest occurrence of any key wins via `seen_keys`, so
     deepest-level entries are only applied when no younger occurrence
     exists.
  The divergence is documented at `applicator.rs:319-327`. **Decision
  required**: keep as model divergence (current henyey behavior) vs.
  add an explicit deepest-level branch to track the spec verbatim.
  Recommend keeping as model divergence — the henyey applicator is
  semantically equivalent and avoids cross-crate `LedgerTxn` coupling.

- **§6.1 / INV-B4 (Effective merge protocol — shadow inclusion).**
  Spec: `protocolVersion := max(old.ver, new.ver,
  any shadow.ver < FIRST_PROTOCOL_SHADOWS_REMOVED)`. Henyey:
  `protocolVersion := max(old.ver, new.ver)` — shadow versions are not
  included (`merge.rs:1071-1102`). Under henyey's P24+ scope shadows are
  always empty, so the third operand is always vacuous and the rule
  collapses to henyey's implementation. Textually divergent but
  behaviorally equivalent within scope. Documented at
  `merge.rs:1066-1070`. **Decision required**: accept as P24+ waiver
  (recommended) vs. add the shadow-version max with a `debug_assert!`
  that shadows are empty (cleaner but non-functional).

---

## Recommendations

1. **Fix the two `§10.6` → `§10.5` anchor typos** in `snapshot.rs`
   (lines 900 and 1718). Trivial, no behavioral impact.
2. **Add an explicit `EvictionResultCandidates::isValid` analogue**
   (§12.5). Today the henyey scan is invoked synchronously by the
   ledger close, so the spec's cross-ledger invalidation hazard does
   not arise — but if eviction ever moves to a true background thread
   pool, this check would be required. Track as a follow-up issue
   only if/when background eviction is reintroduced.
3. **Add a `noteEmptyMergeOutput` analogue** to the
   `BucketMergeMap` (§8.1). Today an empty merge short-circuits at the
   merge-output construction site, so the spec's "future-skipping"
   optimization is implicit. Low impact; consider adding for
   memory-hygiene parity.
4. **Reconcile INV-B12 (deepest-level LIVE = INIT)** by either
   (a) accepting the documented model divergence as a permanent design
   choice and noting it in `PARITY_STATUS.md`, or
   (b) adding an explicit branch in `BucketApplicator::advance` for
   deepest-level LIVE. Recommend (a).
5. **Add a `// Spec:` anchor** on the combined-bucketList-hash code in
   `crates/history/src/replay/execution.rs:468` pointing to
   `BUCKETLISTDB_SPEC §4.7` / `INV-B15`. The implementation is correct
   but discoverability is poor (lives in a different crate from the
   spec primary mapping).
