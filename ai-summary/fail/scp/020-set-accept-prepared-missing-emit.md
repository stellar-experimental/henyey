# H-020: set_accept_prepared Silent Return Without Emit on Broken Invariant

**Date**: 2025-07-24
**Crate**: scp
**Severity**: LOW
**Hypothesis by**: copilot

## Expected Behavior

After `set_accept_prepared` updates protocol state (prepared, prepared_prime, phase), it should always emit a new statement reflecting the updated state. stellar-core's `setAcceptPrepared` always falls through to `emitCurrentStateStatement()` regardless of whether `mCommit && mHighBallot` evaluates to true.

## Mechanism

In Henyey (`state_machine.rs:106-131`), after updating prepared/prepared_prime:
```rust
if let Some(commit) = &self.commit {
    if let Some(high_ballot) = &self.high_ballot {
        // ... additional state updates ...
        self.emit_current_state(ctx);
    }
    // If commit.is_some() but high_ballot.is_none(): returns did_work WITHOUT emitting
    return did_work;
}
// Falls through to emit_current_state if commit is None
self.emit_current_state(ctx);
```

When `commit.is_some()` but `high_ballot.is_none()`, Henyey returns early without calling `emit_current_state`. In stellar-core (`BallotProtocol.cpp:889-923`), the equivalent code path:
```cpp
if (mCommit && mHighBallot) { ... }
emitCurrentStateStatement();
```
The `emitCurrentStateStatement()` is always called regardless of the `mCommit && mHighBallot` condition.

## Attack Vector

If the invariant `commit.is_some() ⟹ high_ballot.is_some()` is ever violated, Henyey would silently fail to broadcast its updated state. An attacker who can somehow induce this broken invariant state would cause Henyey to diverge from stellar-core: stellar-core would emit a new statement while Henyey would not, potentially stalling consensus progress for other nodes waiting for this node's updated statement.

## Target Code

- `crates/scp/src/ballot/state_machine.rs:106-131` — `set_accept_prepared` early return path
- `stellar-core/src/scp/BallotProtocol.cpp:889-923` — `setAcceptPrepared` unconditional emit

## Evidence

The structural difference is confirmed by code inspection. Henyey's `if let Some(commit)` + inner `if let Some(high_ballot)` creates a path where commit exists without high_ballot, leading to early return without emit. stellar-core has no such early return path.

## Anti-Evidence

1. The invariant `commit.is_some() ⟹ high_ballot.is_some()` is maintained throughout the entire ballot protocol:
   - `set_confirm_prepared` (line 252-254): sets commit only when high_ballot is already set
   - `set_accept_commit` (line 341-342): sets both commit and high_ballot atomically
   - `set_confirm_commit` (line 412-413): sets both atomically
   - No code path clears high_ballot while leaving commit set
2. stellar-core also maintains this invariant (`mCommit` and `mHighBallot` are always set together).
3. The early return path is dead code — it can never execute under correct protocol operation.
4. Even if triggered, the advance_slot caller's `send_latest_envelope` would attempt to send the envelope (though it would send the OLD envelope, not a new one reflecting the state change).

---

## Review

**Verdict**: NOT_VIABLE
**Severity**: NONE
**Date**: 2025-07-24
**Reviewed by**: copilot

## Trace Summary

The code difference is real and is a structural divergence from stellar-core. However, it exists on an unreachable code path due to a consistently maintained invariant.

## Code Paths Examined

- `crates/scp/src/ballot/state_machine.rs:99-131` — `set_accept_prepared` full implementation
- `crates/scp/src/ballot/state_machine.rs:227-264` — `set_confirm_prepared` (only place commit is set in PREPARE phase)
- `crates/scp/src/ballot/state_machine.rs:321-363` — `set_accept_commit` (sets commit + high_ballot together)
- `crates/scp/src/ballot/state_machine.rs:405-425` — `set_confirm_commit` (sets commit + high_ballot together)
- `stellar-core/src/scp/BallotProtocol.cpp:889-923` — upstream `setAcceptPrepared`
- `stellar-core/src/scp/BallotProtocol.cpp:1047-1091` — upstream `setConfirmPrepared` (same invariant)

## Findings / Why It Failed

1. **Invariant is universally maintained.** Every code path that sets `commit` also sets `high_ballot` (or sets commit only when high_ballot already exists). There is no path to reach `commit.is_some() && high_ballot.is_none()`.
2. **Dead code path.** The early return without emit cannot execute, making the behavioral difference theoretical only.
3. **stellar-core also relies on this invariant.** The upstream code has `dbgAssert` checks that `mCommit` implies `mHighBallot` in various places, confirming both implementations consider this condition impossible.
4. **Defense in depth only.** The Henyey code could be simplified to remove this dead path (or add an unreachable!() assertion), but it does not constitute a vulnerability.
