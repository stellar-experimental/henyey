# False-Positive Suppression Rules

These are the **10 most common false-positive patterns** from prior audits
(97.5% false-positive rate — 195 of 200 findings were invalid). Before
accepting any finding, verify it does not fall into one of these categories.

## Rule 1: Behavior that matches stellar-core

If the henyey code does the same thing as the corresponding stellar-core code,
it is **parity**, not a bug. This is the single most common false-positive
pattern (63% of all prior FPs).

**Before accepting**: Read the upstream `.cpp`/`.h` in `stellar-core/src/` using
the crate-to-upstream mapping. If the behavior matches, reject the finding.

Examples of correct parity that auditors previously flagged:
- Floating-point price comparison in offer ordering (matches `isBetterOffer`)
- Plain `i64` arithmetic without overflow checks (matches stellar-core C++)
- `Ed25519SignedPayload` signer ordering rules
- Medium threshold for `SetOptions` flag changes
- Non-ASCII `ManageData` key rejection (only printable ASCII 0x20-0x7E)
- Singleton self-quorum for SCP Externalize statements

## Rule 2: HashMap/HashSet in non-consensus paths

Nondeterministic iteration is **only** a finding if the iteration order
affects consensus output (tx_set_result_hash, bucket_list_hash, ledger header
fields, order of ledger entry writes to the bucket list).

NOT a finding in: caching/dedup, TransactionMeta/LedgerCloseMeta, logging,
metrics, test helpers, data structures that feed into sorted output.

## Rule 3: `unwrap()` / `expect()` on protocol invariants

Only a finding if attacker-controlled input can trigger it in production.
NOT a finding when: guaranteed by protocol constraints, field always `Some`
during access phase, documented with `// INVARIANT:` comment, or data from
internally-validated sources.

## Rule 4: Dead code / test-only paths

Code never called in production is not a production vulnerability. Check for:
`#[allow(dead_code)]`, `#[cfg(test)]`, library APIs with no production callers.
**Before accepting**: Search for production callers. Zero callers = not a finding.

## Rule 5: Validation at a different layer

Input may be validated at a layer not visible in a single file. Common boundaries:
- Herder layer: slot range checks before SCP
- Overlay frame decoder: MAX_MESSAGE_SIZE bounds
- HTTP framework: body size limits before RPC handler
- Transaction validation phase: signatures, fees, preconditions
- Soroban host: entry-count limits

**Before accepting**: Trace the production call chain back to the entry point.

## Rule 6: TransactionMeta is not consensus-critical

TransactionMeta and LedgerCloseMeta are NOT part of ledger hash or
tx_set_result_hash. Nondeterministic ordering in meta does not affect consensus.

## Rule 7: Simulation crate is test infrastructure

The `simulation` crate is not deployed in production. Only CRITICAL findings.
Similarly, `work` is an internal async task scheduler — not consensus-critical.

## Rule 8: "Unbounded" allocations with implicit bounds

Many data structures are constrained by: authenticated peer count, flow control
windows, protocol limits (max ops per tx, max signers), network size, total XLM
supply. **Before accepting**: Identify the actual bound at a higher layer.

## Rule 9: Protocol < 24 is out of scope

Only protocol 24+ is supported. Findings about earlier protocols are N/A.

## Rule 10: `Limits::none()` after frame-level size check

Using `Limits::none()` for XDR deserialization is correct when input is already
bounded by frame-level `MAX_MESSAGE_SIZE`. stellar-core similarly does not apply
secondary limits. Double-limiting would diverge from upstream.

## Code Annotations

The codebase uses two structured annotation patterns:

- **`// SECURITY: <what> at <where>`** — Input validated at a different layer.
- **`// INVARIANT: <why>`** — Documents why an unwrap/panic is unreachable.

Read these annotations before flagging annotated code.
