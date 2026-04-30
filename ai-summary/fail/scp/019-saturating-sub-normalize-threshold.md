# H-019: saturating_sub vs Unsigned Wrapping in normalize_quorum_set_with_remove

**Date**: 2025-07-24
**Crate**: scp
**Severity**: LOW
**Hypothesis by**: copilot

## Expected Behavior

When `normalize_quorum_set_with_remove` removes the local node from a quorum set's validator list, the threshold should be reduced by the number of occurrences removed. If removal count exceeds threshold, the resulting quorum set should become unsatisfiable.

## Mechanism

Henyey uses `saturating_sub` (line 405 of `quorum.rs`):
```rust
quorum_set.threshold = quorum_set.threshold.saturating_sub(removed_count as u32);
```

stellar-core uses unsigned integer subtraction (`QuorumSetUtils.cpp:146`):
```cpp
qSet.threshold -= uint32(v.end() - it_v);
```

If `removed_count > threshold` (degenerate quorum set with a node listed more times than the threshold), Henyey saturates to 0 while stellar-core wraps to ~UINT32_MAX.

## Attack Vector

An attacker constructs a degenerate quorum set where a node ID appears more times in the validator list than the threshold value. After normalization, Henyey produces threshold=0 (treated as unsatisfiable by `is_quorum_slice`) while stellar-core produces a wrapped large threshold (also unsatisfiable, but for a different reason — can never accumulate enough votes). A downstream check that tests `threshold == 0` specifically (rather than satisfiability) could diverge.

## Target Code

- `crates/scp/src/quorum.rs:401-405` — `saturating_sub` for threshold reduction
- `stellar-core/src/scp/QuorumSetUtils.cpp:146` — unsigned subtraction for threshold

## Evidence

The code difference is confirmed by inspection. `saturating_sub(3u32)` on threshold=2 yields 0; unsigned `2 - 3` yields 4294967295.

## Anti-Evidence

1. Valid quorum sets never have a single node appearing more times than the threshold.
2. Quorum set validation rejects malformed sets before they reach normalization.
3. Both threshold=0 and threshold=UINT32_MAX produce the same observable result: `is_quorum_slice` returns false (unsatisfiable).
4. No downstream code checks for specific threshold values after normalization — only satisfiability matters.
5. The `normalize_quorum_set_with_remove` is called with `Some(local_node_id)`, and a sane local node appears at most once per validator list level.

---

## Review

**Verdict**: NOT_VIABLE
**Severity**: NONE
**Date**: 2025-07-24
**Reviewed by**: copilot

## Trace Summary

The difference is real but unreachable in practice and produces identical observable behavior for all valid inputs.

## Code Paths Examined

- `crates/scp/src/quorum.rs:374-433` — `normalize_quorum_set_with_remove` implementation
- `crates/scp/src/quorum.rs:90-122` — `is_quorum_slice` threshold=0 returns false
- `stellar-core/src/scp/QuorumSetUtils.cpp:137-174` — `normalizeQSetSimplify` implementation
- `stellar-core/src/scp/LocalNode.cpp:93-122` — `isQuorumSliceInternal` uint32 wrapping on decrement from 0

## Findings / Why It Failed

1. **Degenerate input required.** A node appearing more times than threshold in a single validator list violates quorum set validity invariants enforced upstream by `is_quorum_set_sane`.
2. **Same observable outcome.** Both threshold=0 (Henyey) and threshold=UINT32_MAX (stellar-core) make `isQuorumSlice`/`is_quorum_slice` return false — the quorum set is unsatisfiable either way.
3. **No specific-value checks.** After normalization, threshold is only used for counting in `is_quorum_slice`. No code branches on `threshold == 0` vs `threshold > validators + inner_sets`.
4. **`saturating_sub` is strictly safer.** It avoids undefined-behavior-adjacent unsigned overflow, producing a deterministic minimum instead of a wrapped maximum. This is a robustness improvement, not a divergence.
