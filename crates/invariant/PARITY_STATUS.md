# Invariant Manager Parity Status

This crate implements stellar-core's `InvariantManagerImpl` subsystem for runtime
integrity checks during ledger close.

## Framework

| Feature | stellar-core | henyey | Notes |
|---------|-------------|--------|-------|
| InvariantManager | ✅ | ✅ | Register, enable (regex), check_on_operation_apply |
| Strict/non-strict failure handling | ✅ | ✅ | Strict = panic, non-strict = log |
| Regex-based enable | ✅ ECMAScript | ✅ Rust regex | Rust regex lacks lookaheads (documented divergence) |
| INVARIANT_CHECKS config | ✅ | ✅ | Translated from compat config |
| INVARIANT_EXTRA_CHECKS config | ✅ | ✅ | Validated (cannot be true on validator) |
| STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY | ✅ | ⚠️ Parsed only | Snapshot hook not yet wired |
| /info endpoint (invariant failures) | ✅ | ✅ | JSON failure map exposed |

## Invariants

| Invariant | stellar-core | henyey | Strict? | Notes |
|-----------|-------------|--------|---------|-------|
| AccountSubEntriesCountIsValid | ✅ | ✅ | No | Pool share trustlines count as 2 |
| LedgerEntryIsValid | ✅ | ⚠️ Partial | No | Missing: Soroban entries, asset validity, claim predicates |
| SponsorshipCountIsValid | ✅ | ✅ | No | Full sponsorship accounting |
| ConservationOfLumens | ✅ | ❌ | No | Deferred (needs header access) |
| LiabilitiesMatchOffers | ✅ | ❌ | No | Deferred |
| BucketListIsConsistentWithDatabase | ✅ | ❌ | Yes | Deferred (bucket-apply hook) |
| BucketListStateConsistency | ✅ | ❌ | Yes | Deferred (snapshot hook) |
| ConstantProductInvariant | ✅ | ❌ | Yes | Deferred |
| OrderBookIsNotCrossed | ✅ | ❌ | Yes | Deferred (special-case hook) |
| EventsAreConsistentWithEntryDiffs | ✅ | ❌ | No | Deferred |

## Hook Points

| Hook | stellar-core | henyey | Notes |
|------|-------------|--------|-------|
| Per-operation (after apply) | ✅ | ✅ | In `apply.rs` after delta+event finalization |
| Per-ledger (commit) | ✅ | ❌ | Deferred |
| Bucket-apply | ✅ | ❌ | Deferred |
| Snapshot (periodic) | ✅ | ❌ | Deferred (config parsed, timer not wired) |

## Known Divergences

1. **Regex engine**: stellar-core uses ECMAScript regex (supports lookaheads);
   henyey uses Rust's `regex` crate (no lookaheads). Patterns using `(?=...)` or
   `(?!...)` will fail to compile with an error message.

2. **LedgerEntryIsValid partial**: The following checks are not yet implemented:
   - Soroban ContractData/ContractCode/TTL entry validation
   - Asset validity (issuer format, code length)
   - ClaimableBalance claim predicate validation
   - ConfigSetting entry validation
