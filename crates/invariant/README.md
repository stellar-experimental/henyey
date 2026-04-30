# henyey-invariant

Runtime invariant checks for the henyey stellar-core implementation.

## Overview

This crate provides the `InvariantManager` and `Invariant` trait, mirroring
stellar-core's `InvariantManagerImpl` / `Invariant` subsystem. Invariants are
read-only checks that run after each operation apply to detect ledger corruption
early.

## Architecture

- **`InvariantManager`**: Registry of invariants with regex-based enable, failure
  tracking, and JSON info reporting. Thread-safe (`Send + Sync`).
- **`Invariant` trait**: Interface for individual checks. Each invariant receives
  an `OperationDelta` (created/updated/deleted entries) and returns `Ok(())` or
  `Err(message)`.
- **Failure modes**: Strict invariants panic on failure (matching stellar-core's
  `throw InvariantDoesNotHold`). Non-strict invariants log and increment a counter.

## Implemented Invariants

| Name | Description | Strict? |
|------|-------------|---------|
| `AccountSubEntriesCountIsValid` | Verifies account `num_sub_entries` matches actual sub-entry count | No |
| `LedgerEntryIsValid` | Validates structural integrity of ledger entries | No |
| `SponsorshipCountIsValid` | Verifies `num_sponsoring`/`num_sponsored` accounting | No |

## Configuration

In henyey's native TOML config:

```toml
[invariants]
checks = [".*"]           # Enable all invariants (regex patterns)
extra_checks = false      # Cannot be true on validator nodes
snapshot_frequency_secs = 300
```

In stellar-core compat config:

```toml
INVARIANT_CHECKS = ["AccountSubEntriesCountIsValid", "SponsorshipCountIsValid"]
INVARIANT_EXTRA_CHECKS = false
STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY = 300
```

## Dependencies

This crate depends only on `stellar-xdr`, `tracing`, `serde_json`, and `regex`.
It has no dependency on other henyey crates to avoid circular dependencies.
