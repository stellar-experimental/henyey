# H-021: statement_quorum_set_map Local Node Fallback Enables Early Quorum Detection

**Date**: 2025-07-24
**Crate**: scp
**Severity**: MEDIUM
**Hypothesis by**: copilot

## Expected Behavior

In `federated_accept` and `federated_ratify`, the quorum check should use quorum sets derived from nodes' latest statements. A node without a recorded statement should not have its quorum set available for the `is_quorum` pruning loop.

## Mechanism

Henyey's `statement_quorum_set_map` (`statements.rs:289-303`) includes a fallback:
```rust
if !map.contains_key(ctx.local_node_id) {
    map.insert(ctx.local_node_id.clone(), ctx.local_quorum_set.clone());
}
```

This ensures the local node's quorum set is ALWAYS available in the map, even if the local node hasn't emitted a ballot statement yet. In stellar-core, `isQuorum` gets quorum sets via `qfun(map.find(nodeID)->second->getStatement())` which requires `nodeID` to exist in `mLatestEnvelopes`. If the local node hasn't sent a statement, `map.find(localID)` would not find it, and the node would be pruned from the quorum calculation.

However, `federated_accept`/`federated_ratify` build the `supporters` set from `self.latest_envelopes`. If the local node hasn't emitted a statement, it won't be in `supporters` and won't be passed to `is_quorum` — so the fallback quorum set would never be looked up.

## Attack Vector

If there exists a code path where the local node IS in `latest_envelopes` (and thus in supporters) but `statement_quorum_set` returns None for the local node's own statement, Henyey would use the fallback quorum set and potentially form a quorum where stellar-core would prune the local node (due to missing quorum set) and fail to find a quorum.

This could happen if the local node's quorum set changes between emitting its statement and processing a new envelope: the hash in the statement no longer matches the current `ctx.local_quorum_set`, and `get_quorum_set_by_hash` doesn't have the old set cached.

## Target Code

- `crates/scp/src/ballot/statements.rs:289-303` — `statement_quorum_set_map` local fallback
- `crates/scp/src/ballot/statements.rs:137-161` — `resolve_quorum_set` hash check for local node
- `stellar-core/src/scp/BallotProtocol.cpp:220-222` — `qfun` in `isQuorum` uses map lookup

## Evidence

The fallback insertion at line 299-301 is not present in stellar-core's equivalent code path. If reached, it provides a quorum set that might not match what the local node's own statement advertises.

## Anti-Evidence

1. **Unreachable in practice.** For the local node's quorum set to NOT be resolved:
   - `resolve_quorum_set` first checks `node_id == ctx.local_node_id` and returns `ctx.local_quorum_set` if the hash matches (line 144-148).
   - The hash would only NOT match if the local node changed its quorum set after emitting the statement.
   - Quorum set changes require restarting the node in both stellar-core and Henyey.
2. **Even if reached, supporters gate access.** The local node must be in `latest_envelopes` to be in supporters. If it's there, `resolve_quorum_set` at line 144-148 handles it (the current quorum set matches because it doesn't change at runtime).
3. **stellar-core has the same effective behavior.** In stellar-core, `Slot::getQuorumSetFromStatement` for the local node always succeeds because the local node's quorum set is always registered with the SCP driver.
4. **The fallback is defense-in-depth.** It prevents the local node from being erroneously pruned from quorum checks due to a cache miss, which would be a liveness issue, not a safety issue.

---

## Review

**Verdict**: NOT_VIABLE
**Severity**: NONE
**Date**: 2025-07-24
**Reviewed by**: copilot

## Trace Summary

The fallback is a defensive addition that can only activate in states that don't occur in production. When it could theoretically activate (quorum set change mid-slot), it provides correct behavior (using the current quorum set) rather than incorrect behavior (pruning self from quorum).

## Code Paths Examined

- `crates/scp/src/ballot/statements.rs:289-303` — `statement_quorum_set_map` with fallback
- `crates/scp/src/ballot/statements.rs:116-135` — `statement_quorum_set` routing by pledge type
- `crates/scp/src/ballot/statements.rs:137-161` — `resolve_quorum_set` multi-tier resolution
- `crates/scp/src/ballot/statements.rs:305-334` — `federated_accept` uses quorum set map
- `crates/scp/src/quorum.rs:143-172` — `is_quorum` pruning loop calls get_quorum_set
- `stellar-core/src/scp/BallotProtocol.cpp:199-238` — upstream `isQuorum` with qfun closure
- `stellar-core/src/scp/Slot.cpp` — `getQuorumSetFromStatement` always resolves local node

## Findings / Why It Failed

1. **Quorum sets don't change at runtime.** Both implementations initialize the local quorum set at startup. It never changes during a node's lifetime, so the hash in emitted statements always matches `ctx.local_quorum_set`.
2. **`resolve_quorum_set` handles the local node directly.** Line 144-148 returns the local quorum set for the local node's own statements without going through the cache, making the fallback at line 299-301 redundant but harmless.
3. **The supporters set gates access.** Even with the fallback quorum set in the map, `is_quorum` only calls `get_qs` for nodes in the `supporters` set. The local node is only in supporters if it has a statement in `latest_envelopes`, which means `resolve_quorum_set` would have already succeeded for it.
4. **Defensive coding pattern.** The fallback ensures robustness against future code changes or edge cases where the resolution chain might fail. It does not create a divergence from stellar-core because the condition for needing it (local node in supporters but missing from quorum set map) cannot occur.
