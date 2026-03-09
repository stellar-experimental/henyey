# Ledger Close v2: Security Analysis

This document evaluates the security implications of each design decision in
`ledger-close-v2-design.md`. The threat model is a production Stellar validator on mainnet:
an adversary can submit arbitrary transactions, attempt to influence SCP nominations, observe
all network traffic, and potentially control a minority of validators.

The ultimate correctness check for a validator is the **ledger hash**: any execution error that
produces a different hash than the rest of the quorum is detected by consensus and the node
falls behind. However, "detected by consensus" is not the same as "safe" — a node that
consistently produces wrong hashes is useless as a validator, and an error that produces the
*same* wrong hash across multiple nodes simultaneously is not caught by consensus at all.

---

## S1: Stale Cache Read Mid-Ledger (CRITICAL)

**Affects:** WarmCache, OfferSellerStore, MaterializedView

### The risk

WarmCache and OfferSellerStore are populated from the *previous ledger's committed state*. They
are updated at ledger end, not incrementally during execution. If a TX in ledger N modifies
account A (e.g., deducts a fee), and a later TX in ledger N reads account A's balance through
WarmCache instead of through `LedgerStateManager`, it would see ledger N-1's balance — the fee
deduction would be invisible. The second TX could then succeed using funds that were already spent.

This is not a hypothetical: fee deduction happens in a bulk pass before execution, and the same
account may appear as both a fee source and a payment source in the same ledger. If any cache
layer returns the pre-fee balance to the payment operation, the account could be double-spent
within a single ledger.

### Severity

**Critical.** An implementation error here enables double-spend within a single ledger. The
wrong ledger hash would be broadcast and rejected by quorum, so the node would stall rather
than permanently corrupt mainnet state — but the double-spend would succeed locally and the
validator would become non-functional until it recatchups.

### Mitigation required

The read priority chain (design doc §Read Path) must be **enforced by the type system or
encapsulated behind a single accessor**, not by convention. Callers must not be able to reach
WarmCache or OfferSellerStore directly — they must go through `MaterializedView::get()` (or
the equivalent `load_account` / `load_trustline` entry points in `TransactionExecutor`), which
always checks `LedgerStateManager` first.

Specific invariants that must hold:

1. `LedgerStateManager` is checked before every other cache layer without exception.
2. WarmCache and OfferSellerStore are **never written to** during execution — only read. They
   are updated only after the full ledger delta is committed (post-refund, post-eviction).
3. `MaterializedView::baseline` is frozen at ledger start and never updated during execution.
   Only `delta_overlay` reflects in-flight modifications.
4. After `apply_transactions` returns, WarmCache and OfferSellerStore are updated from the
   final committed delta — **including** refunds and eviction changes, not just the raw TX
   outputs.

**Test requirement:** Include a regression test with a TX that deducts fees from account A,
followed by a TX that attempts to spend the same funds from A in the same ledger. Verify the
second TX fails with `txINSUFFICIENT_BALANCE`, not with a wrong hash.

---

## S2: Async Persist — Snapshot Consistency (HIGH)

**Affects:** Component 6 (Async Persist)

### The risk

The async persist design runs bucket list delta application as a background `PersistTask`
overlapping with the next SCP nomination round. The `SnapshotHandle` for ledger N+1 must be
constructed from the bucket list state *after* ledger N's delta is applied. If the snapshot
is constructed before the persist task completes, ledger N+1 executes against ledger N-1's
state — an entire ledger of changes is invisible to execution. Every read would return stale
values; every write would overwrite committed state.

The design states: *"PersistTask for ledger N runs during SCP round N+1 nomination, completing
before tx_set N+1 is externalized."* This is a **timing assumption**, not a synchronization
guarantee. SCP can be fast:

- During catchup (no real-time SCP, just replaying history), rounds complete back-to-back.
  PersistTask cannot keep up; there is no nomination gap.
- On a fast network or under low-latency conditions, SCP rounds can close in under 1 second.
  A slow disk (e.g., under write pressure from merge tasks) could cause PersistTask to miss
  the window.

### Severity

**High.** If snapshot construction races with PersistTask, the node executes ledger N+1 against
stale state, producing a wrong hash, and stalls. With a persistent timing bug, the node could
enter an infinite stall/catchup loop.

More subtly: if the timing assumption holds 99.9% of the time but fails under load, the node
has a latent crash bug that only manifests under production stress — the hardest kind to diagnose.

### Mitigation required

1. **Hard synchronization, not timing.** `SnapshotHandle::new()` for ledger N+1 must block
   until PersistTask has confirmed completion of ledger N's delta. Use a `tokio::sync::watch`
   channel or a `Notify` that PersistTask signals when done. `SnapshotHandle` construction waits
   on this signal before proceeding.

2. **Catchup mode must bypass async persist entirely.** During catchup (no SCP nomination gap),
   persist synchronously. The async optimization only applies to live consensus mode where SCP
   rounds are measured in seconds.

3. **Backpressure must be a hard barrier, not a soft heuristic.** The design says "if the delta
   channel depth > 1, revert to synchronous persist." This must be guaranteed: if PersistTask is
   behind, execution halts until it catches up — not merely slows down.

---

## S3: Speculative Prefetch — DoS and Cache Pollution (MEDIUM)

**Affects:** Component 3 (SpeculativePrefetch)

### The risk

Speculative prefetch triggers bucket list scans during SCP nomination, before any tx_set is
externalized. An attacker who can influence nominations (e.g., by submitting large tx_sets or
by being a validator themselves) can cause the prefetch scheduler to scan arbitrarily many
entries, consuming I/O bandwidth without those ledgers ever closing.

Specifically:
- An attacker submits a tx_set referencing 10,000 unique accounts. Nomination triggers a
  bucket list scan for all 10,000. The tx_set is not externalized (quorum rejects it). On the
  next nomination round, the attacker submits another 10,000 unique accounts. Each round costs
  ~10,000 × 150μs = 1.5 seconds of I/O that produces nothing.
- A Byzantine validator can nominate arbitrary tx_sets (within protocol limits) repeatedly.

Additionally, speculative prefetch writes to WarmCache. A carefully constructed series of
nominations (even if never externalized) could evict hot entries from WarmCache — replacing
200K genuine hot entries with the attacker's cold entries — degrading performance for real
ledger execution for many subsequent ledgers.

### Severity

**Medium.** This is a performance degradation attack, not a correctness attack. Consensus
ensures that only valid, quorum-approved tx_sets are externalized. The attacker can at most
slow down the node, not corrupt its state.

However, on a mainnet validator, performance degradation is a liveness concern: a slow node
may miss SCP nomination windows and fail to contribute to consensus.

### Mitigations required

1. **Only prefetch for your own nomination.** The prefetch scheduler should only trigger for
   tx_sets that the *local* node nominated (i.e., constructed from its own mempool), not for
   all received nominations. This eliminates the external attacker vector — an attacker can't
   trigger your prefetch without controlling your mempool.

2. **Separate speculative tier in WarmCache.** Speculative prefetch writes to a "speculative
   tier" with lower eviction priority than the confirmed tier. Entries in the speculative tier
   are promoted to the main cache only if their tx_set is externalized. If the tx_set is not
   externalized within K rounds, the speculative tier entries are discarded without evicting main
   cache entries.

3. **Use random eviction in WarmCache, not LRU.** LRU creates a deterministic eviction path:
   an adversary who submits TXs referencing N+1 unique accounts (where N is the cache capacity)
   can guarantee eviction of any specific hot entry in a predictable number of rounds. With
   random eviction, the attacker can only increase the *probability* of eviction, not guarantee
   it. This matches stellar-core's own `RandomEvictionCache` policy. The performance difference
   is negligible for working sets that fit within the cache capacity (the steady-state case).

4. **Rate-limit prefetch I/O.** Cap the speculative prefetch at N keys per SCP round (e.g.,
   2× the expected tx_set size for normal traffic). If the candidate tx_set is unusually large,
   prefetch only the first N keys and let the rest be cache-through hits.

---

## S4: Delta Coalescing Correctness (HIGH)

**Affects:** MaterializedView, LedgerDelta

### The risk

`MaterializedView::delta_overlay` applies coalescing rules:
- Create + Update → Create (with latest value)
- Create + Delete → absent (removed from overlay)
- Update + Delete → Delete

These rules are correct *only* if the Create/Update/Delete sequence faithfully represents what
actually happened to the entry within the ledger. An incorrect coalescing rule could:

- **Create + Delete → absent** is correct for a new entry created and then deleted in the same
  ledger (net effect: entry never existed). But if the entry *already existed* before the ledger
  (it was in the bucket list), and ledger N first deletes it (Delete) and then recreates it
  (Create), the coalesced result must be Create with the new value — not treated as "net new"
  (which would incorrectly suppress the Delete in `LedgerEntryChanges`).

- **Soroban TTL entries** are modified in the same delta as their corresponding ContractData
  entries. If a ContractData entry is archived (deleted from classic state) and its TTL entry
  is simultaneously updated, the coalescing must handle the cross-entry dependency correctly.

- **Hot-archive restore** (protocol 23+ with hot archive): An entry restored from the hot
  archive appears as a Create in the delta even though it previously existed. If coalescing
  treats this as "new entry" and later a Delete arrives, it becomes absent — but the hot archive
  still holds the original entry, and the node's understanding of the entry's history is now
  wrong.

### Severity

**High.** Incorrect coalescing produces wrong `LedgerEntryChanges` (transaction metadata),
which is part of the ledger hash. A metadata hash mismatch causes consensus failure. More
subtly, incorrect coalescing in the execution path (not just metadata) produces wrong execution
outcomes.

### Mitigation required

1. **Track entry provenance in delta_overlay.** Each entry in `delta_overlay` must carry a flag
   indicating whether it was pre-existing (existed in bucket list before this ledger) or new
   (Created this ledger). Coalescing behavior differs based on provenance:
   - Pre-existing + Delete = Delete (entry existed, now gone)
   - New + Delete = absent (entry never committed, no trace)
   - Pre-existing + Delete + Create = Update (delete-then-recreate = update from metadata perspective)

2. **The existing `LedgerDelta` coalescing logic is the battle-tested reference.** MaterializedView
   must use the same coalescing rules, not a reimplementation. Consider sharing code rather than
   duplicating logic.

3. **Regression test:** Run every ledger in the protocol 25 range through MaterializedView
   and verify that `LedgerEntryChanges` output is byte-identical to the current implementation.

---

## S5: Non-Determinism Across Validators (HIGH)

**Affects:** All caching layers

### The risk

Stellar consensus requires that all validators in a quorum produce identical execution results
for the same tx_set. Any caching layer that produces different results depending on node-local
state (cache contents, timing, OS page cache) introduces non-determinism risk.

For read-through caches (cache miss → same result as bucket list lookup), this is safe: cache
presence affects performance but not correctness. The concern is with any optimization that:

1. **Short-circuits existence checks.** If a cache returns "entry not found" when it is present
   in the bucket list (false negative), the TX might succeed where it should fail (e.g., an
   account-not-found check passes because the cache incorrectly returns absent).

2. **Serves an outdated committed value.** If WarmCache holds an entry from ledger N-2 (missed
   update in ledger N-1), and the bucket list holds the ledger N-1 value, a cache hit would
   produce a different result than a cache miss. Two validators with different cache states would
   produce different execution results.

3. **WarmCache invalidation race.** If WarmCache is updated concurrently with execution (rather
   than only at ledger end), a TX executing on one thread might see an updated entry that a TX
   on another thread hasn't seen yet — non-deterministic within the same ledger.

### Required invariants

1. **WarmCache false negative is impossible.** A cache miss must never falsely indicate "absent"
   for an entry that exists in the bucket list. The only valid false negative is for an entry
   that was deleted in a previous ledger and correctly evicted from WarmCache. To verify: the
   bucket list is always the final authority. Any lookup that returns None from WarmCache must
   proceed to the bucket list.

2. **WarmCache is updated atomically at ledger end.** No WarmCache writes occur during execution.
   All writes occur in a single serialized pass after `apply_transactions` returns with the final
   committed delta. This guarantees that all validators applying the same delta will converge to
   the same WarmCache state for the next ledger (modulo eviction policy, which only affects
   performance, not correctness).

3. **Eviction policy must not affect correctness.** LRU eviction policy removes entries from
   WarmCache but does not remove them from the bucket list. A node with a smaller WarmCache (due
   to more eviction) must produce identical execution results to a node with a larger cache —
   just with more bucket list scans.

---

## S6: Memory Pressure and OOM Risk (MEDIUM)

**Affects:** WarmCache, OfferSellerStore, InMemorySorobanState, OfferIndex

### The risk

The v2 design adds persistent in-memory state on top of existing persistent state:

| Structure | Existing | New (v2) |
|-----------|----------|----------|
| OfferIndex | ~911K offers × ~200B = ~180MB | — |
| InMemorySorobanState | ~large (all contract data) | — |
| WarmCache | — | ~100MB |
| OfferSellerStore | — | ~300–400MB |
| MaterializedView (per-ledger) | — | ~small, ephemeral |

If the process approaches the OOM threshold, the Linux OOM killer may terminate it mid-execution.
The node restarts, recatchups, and resumes — this is a liveness issue, not a safety issue. But
consistent OOM conditions (e.g., InMemorySorobanState growing unboundedly as new contracts are
deployed) could make the node permanently unstable.

### Mitigations

1. **WarmCache capacity must be configurable and bounded.** The 200K-entry / 100MB default must
   be enforced as a hard cap, not a soft target. LRU eviction must fire *before* the cap is
   exceeded, not after.

2. **OfferSellerStore must evict departed sellers.** The full-coverage design bounds memory by
   mirroring `OfferIndex` membership — entries are evicted when a seller's last live offer is
   deleted. The implementation must enforce this invariant in `update_from_delta` to prevent
   unbounded growth as old sellers leave the DEX.

3. **Monitor total resident set size.** Add a metric emitting the combined size of WarmCache +
   OfferSellerStore + OfferIndex at each ledger close. Alert if total exceeds a configured
   threshold (e.g., 80% of available RAM). This is the first defense against unbounded growth.

---

## S7: Bucket List as Ground Truth (FUNDAMENTAL CONSTRAINT)

All of the above risks share a common mitigation: **the bucket list must always be the final
authority for any ledger entry, and every cache miss must fall through to it without exception.**

This is not a new requirement — it describes the current architecture — but the v2 design
introduces more layers that could silently intercept lookups before they reach the bucket list.
The implementation must ensure that:

1. No cache layer can return a definitive "absent" result that suppresses a bucket list lookup
   (except for entries that were explicitly deleted in the current ledger's delta, which are
   tracked in `delta_overlay`).

2. The bucket list `SnapshotHandle` is always reachable from any execution context, even when
   all cache layers are populated. A code path that can only succeed via the cache and silently
   fails if the cache is cold is a latent bug.

3. During development and testing, intentionally run with caches disabled (or with aggressive
   eviction) and verify hash parity. A correct implementation should produce identical hashes
   regardless of cache state.

---

## Summary: Risk Ratings by Component

| Component | Risk | Severity | Mitigation |
|-----------|------|----------|------------|
| WarmCache stale read | Implementation error → double-spend within ledger | **Critical** | Type-enforced priority; write-only at ledger end |
| OfferSellerStore stale read | Same as WarmCache | **Critical** | Same as WarmCache |
| Async persist snapshot race | Execution against stale state | **High** | Hard synchronization barrier, not timing |
| Delta coalescing error | Wrong metadata hash / wrong execution | **High** | Share coalescing logic with LedgerDelta; provenance tracking |
| Non-determinism across validators | Validator disagreement → liveness failure | **High** | Atomic WarmCache updates; bucket list always reachable |
| Speculative prefetch DoS | Liveness degradation | **Medium** | Own-nomination-only; speculative tier in WarmCache |
| Memory pressure / OOM | Liveness failure | **Medium** | Hard-bounded caches; resident-set metric |

### Implementation order informed by risk

1. **Before any caching layer lands:** Add comprehensive hash-parity tests against the current
   implementation. These tests must run on every commit and gate merges.

2. **WarmCache and OfferSellerStore:** Low algorithmic risk (read-through only), but implement
   with the type-level enforcement (single access point via TransactionExecutor) from day one.
   Do not allow direct cache access from call sites.

3. **Async persist:** Implement the synchronization barrier before wiring up the async path.
   Test explicitly under fast-SCP conditions (simulated catchup pace).
