# Security Audit Context

## Project Identity

Henyey is a **Rust re-implementation of stellar-core** (v25.x / protocol 25).
The stellar-core C++ source is available as a git submodule at `stellar-core/`
(pinned to v25.0.1).

**Behavior that matches stellar-core is correct, not a bug.** Consensus parity
is a hard requirement — every node on the Stellar network must produce
identical ledger state for identical inputs.

Protocol support is 24+ only; do not flag behavior under earlier protocols.

## Severity Scale

| Severity | Criteria |
|----------|----------|
| **HIGH** | Create XLM, steal funds, consensus divergence / chain split, non-quorum-member can crash a node, a single transaction can crash a node, unauthenticated peer can DoS/crash a node (overlay, SCP input) |
| **MEDIUM** | Quorum member can crash a node, quorum member DOS, bad configuration, authenticated-peer-only overlay bugs, RPC-only DoS (max severity for non-consensus RPC bugs) |
| **LOW** | History archive crashes, vulnerabilities affecting only out-of-sync nodes, non-severe metering mismatches, simulation/test crate bugs (max severity for test infrastructure) |
| **INFORMATIONAL** | All other confirmed bugs that are real bugs but not exploitable vulnerabilities (e.g., info disclosure, silent error swallowing with no security impact) |

### Severity Classification Guidelines

These rules resolve common ambiguities:

1. **Consensus divergence is always HIGH.** Any bug where different nodes
   produce different ledger state for the same input — including hash
   mismatches, nondeterministic iteration, or parity deviations — is HIGH
   regardless of how unlikely the trigger is.

2. **Unauthenticated peer DoS is HIGH.** If any connected peer (before or
   after auth) can crash, stall, or exhaust a node's resources, that is HIGH.
   Overlay and SCP input from unauthenticated sources falls here.

3. **Authenticated peer DoS is MEDIUM.** If the attack requires a fully
   authenticated peer (post-handshake), it is MEDIUM — the trust model assumes
   authenticated peers are semi-trusted.

4. **RPC bugs cap at MEDIUM.** RPC is not consensus-critical. Memory
   exhaustion, thread safety, or pagination bugs in RPC endpoints are MEDIUM
   at most. RPC bugs that are merely informational (info disclosure, silent
   error swallowing) are INFORMATIONAL.

5. **Simulation/test crate bugs cap at LOW.** The simulation and work crates
   are not production code. Bugs there are LOW at most, regardless of
   theoretical impact.

6. **History archive bugs are LOW.** Archive-only issues (download
   verification, catchup-only state) affect out-of-sync nodes, not the live
   network.

7. **"Crash the network" means network-wide impact.** A single node crashing
   is HIGH (via criteria #1/#2 above). "Crash the network" in the HIGH
   criteria refers to taking down the entire network, not just one node.

A real bug at any severity — including Informational — should proceed through
the pipeline, not be rejected. Only reject if the bug does not actually exist,
is by design, or is out of scope.

## Out of Scope

- Economic/governance attacks (51%), Sybil, centralization, liquidity impacts
- Malicious validators / v-blocking set — SCP axiom makes this inherent
- Malicious history archives — trust model assumes trusted operators
- Leaked keys / privileged access required
- Transaction ban/dedup avoidance via semantic duplicates — creating tx variants
  that hash-differently but are semantically equivalent (memo variation, muxed
  ID variation, signature permutation, op-source toggling, footprint/auth-entry
  order permutation, etc.) is by design, not a vulnerability
- Test/config file only impacts
- Theoretical issues without concrete exploitation path
- Protocol < 24 bugs (out of scope)
- "Future slot" bugs requiring malicious quorum to externalize bad value
- "Online catchup" bugs premised on re-applying buckets (never happens)
- Previous protocol bugs not present in current protocol
- Wasmi upstream bugs (may be out of scope per carve-out)

## Crate-to-Upstream Mapping

| Crate | Upstream Directory |
|-------|--------------------|
| `tx` | `stellar-core/src/transactions/` |
| `scp` | `stellar-core/src/scp/` |
| `db` | `stellar-core/src/database/` |
| `common` | `stellar-core/src/util/` |
| `crypto` | `stellar-core/src/crypto/` |
| `ledger` | `stellar-core/src/ledger/` |
| `bucket` | `stellar-core/src/bucket/` |
| `herder` | `stellar-core/src/herder/` |
| `overlay` | `stellar-core/src/overlay/` |
| `history` | `stellar-core/src/history/` |
| `historywork` | `stellar-core/src/historywork/` |
| `work` | `stellar-core/src/work/` |
| `app` | `stellar-core/src/main/` |
| `henyey` | `stellar-core/src/main/` (CLI subset) |
| `rpc` | *(no upstream — henyey-specific)* |
| `simulation` | *(no upstream — test infrastructure)* |

## Crate Risk Tiers

| Tier | Crates | Security Model |
|------|--------|----------------|
| **Consensus-critical** | tx, ledger, scp, herder, bucket | Determinism required. Parity mandatory. Bugs cause chain splits, double-spends, or network halts. |
| **Network-facing** | overlay, rpc | Handles untrusted external input. NOT consensus-critical. |
| **Infrastructure** | app, history, historywork, crypto, common, db, clock | Supporting code. Bugs may cause crashes but not consensus divergence directly. |
| **Test/development** | simulation, work | NOT production code. Only CRITICAL findings. |
