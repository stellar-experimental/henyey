# H-022: Auth Cert Clock Skew Causes Spurious Peer Rejection

**Date**: 2025-01-27
**Crate**: overlay
**Severity**: MEDIUM
**Hypothesis by**: claude-opus-4.6

## Expected Behavior
A valid peer with a slightly ahead system clock should be able to connect.
The auth cert expiration check should tolerate reasonable clock skew (a few
seconds) between nodes, since not all validators have perfectly synchronized
clocks.

## Mechanism
In `auth.rs:139`, the expiration check is:
```rust
if self.expiration <= now {
    return Err(OverlayError::AuthenticationFailed("auth cert expired"));
}
```

The cert is created with `expiration = now + 3600s` on the *sender's* clock.
If the receiver's clock is ahead by even 1 second relative to the sender's
clock at the moment of cert creation, a cert created exactly 3599 seconds ago
from the sender's perspective could appear expired to the receiver.

More critically: stellar-core uses `cert.expiration < mApp.timeNow()` (strict
less-than in PeerAuth.cpp:58), while henyey uses `<=` (less-than-or-equal).
This means henyey rejects one additional second boundary where stellar-core
would accept, creating a parity deviation that narrows the effective cert
validity window.

An attacker can exploit this by:
1. Connecting to multiple target nodes simultaneously
2. Sending Hello messages with auth certs whose expiration is `target_now`
   (crafted to be exactly at the boundary)
3. Nodes with slight clock drift reject the connection; nodes without don't

This creates a targeted eclipse attack vector: peers with clocks ahead by
even a few seconds will reject legitimate connections that stellar-core
would accept.

## Attack Vector
1. Attacker identifies a target validator with slightly fast clock (+1-5 seconds)
2. Attacker controls a large portion of the target's potential peer set
3. Legitimate peers create auth certs that are valid in stellar-core but rejected
   by the target henyey node (due to the `<=` vs `<` discrepancy)
4. Target node preferentially connects to attacker-controlled nodes, enabling
   eclipse or consensus delay

## Target Code
- `crates/overlay/src/auth.rs:verify:129-161` — expiration check uses `<=` instead of `<`
- `stellar-core/src/overlay/PeerAuth.cpp:58` — uses strict `<` comparison

## Evidence
- Line 139: `if self.expiration <= now` — off-by-one vs stellar-core's `<`
- No clock-skew tolerance margin in either implementation
- Auth certs are created with exactly 3600s lifetime, no buffer
- `SystemTime::now()` can drift on nodes without NTP or with NTP jitter

## Anti-Evidence
- The difference is exactly 1 second at the boundary — very narrow window
- Most production nodes run NTP with sub-second accuracy
- Cert lifetime is 3600s, so the 1-second difference is rarely hit
- Certs are refreshed well before expiry (stellar-core refreshes at expiration/2)
- This is a determinism/parity issue more than a practical attack

---
## Review
**Verdict**: NOT_VIABLE
**Failed At**: hypothesis
**Reviewed by**: claude-opus-4.6
### Why It Failed
The `<=` vs `<` difference creates a 1-second parity deviation but the attack
window is too narrow to be practically exploitable. Auth certs have a 3600-second
lifetime and are refreshed at the halfway point (1800s). The only exploitable
moment is when a cert is within its final second of life AND the receiver's clock
is faster. With NTP, this window is sub-millisecond. The eclipse scenario requires
controlling the majority of the peer set which is already game-over.

### Lesson Learned
Clock-boundary off-by-one errors in time comparisons are parity bugs (should be
fixed for determinism) but rarely constitute exploitable security vulnerabilities
when the time window is large (hours) relative to the discrepancy (1 second).
