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
| **HIGH** | Create XLM, steal funds, non-quorum-member can crash the network, a single transaction crashes the network |
| **MEDIUM** | Quorum member can crash the network, quorum member DOS |
| **LOW** | History archive crashes, vulnerabilities affecting only out-of-sync nodes, non-severe metering mismatches |
| **INFORMATIONAL** | All other confirmed bugs that are real bugs but not exploitable vulnerabilities |

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
