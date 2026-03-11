# XDR Version Alignment Plan: stellar-xdr 24.0.1 (git) -> 25.0.0 (crates.io)

## Goal

Migrate henyey's workspace `stellar-xdr` dependency from a git revision
(version 24.0.1) to the official crates.io release (version 25.0.0), matching
the version used by soroban-env-host P25. This unifies the XDR types between
henyey and soroban-env-host P25, enabling zero-cost type interoperability and
eliminating ~50 XDR serialization round-trips per Soroban transaction.

## Current State

Three `stellar-xdr` versions coexist in the lockfile:

| Version | Source | Used By |
|---------|--------|---------|
| 24.0.0 | crates.io | soroban-env-host P24 |
| 24.0.1 | git rev `89cc1cb` | henyey workspace (all 14 crates) |
| 25.0.0 | crates.io | soroban-env-host P25 |

The git rev 24.0.1 already contains the full P25 XDR schema (including
BN254 cost types, `ConfigSettingEntry::ContractParallelComputeV0`,
`TransactionMetaV4`, `SorobanTransactionMetaV2`, etc.). Moving to 25.0.0
is essentially moving from an unpublished pre-release to the official release.

## Target State

| Version | Source | Used By |
|---------|--------|---------|
| 24.0.0 | crates.io | soroban-env-host P24 |
| 25.0.0 | crates.io | **henyey workspace + soroban-env-host P25** (unified) |

Cargo will resolve both the workspace's `stellar-xdr = "=25.0.0"` and
soroban-env-host P25's transitive `stellar-xdr = "=25.0.0"` to the **same
crates.io package**, making the Rust types identical. No XDR byte-bridge
needed for P25.

## Migration Steps

### Step 1: Change workspace dependency

**File**: `Cargo.toml` (workspace root)

Change line 34 from:
```toml
stellar-xdr = { git = "https://github.com/stellar/rs-stellar-xdr", rev = "89cc1cbadf1b9a16843826954dede7fec514d8e7", default-features = false, features = ["std", "curr", "serde", "serde_json"] }
```
To:
```toml
stellar-xdr = { version = "=25.0.0", default-features = false, features = ["std", "curr", "serde", "serde_json"] }
```

### Step 2: Remove the `[patch.crates-io]` section

**File**: `Cargo.toml` (workspace root)

Remove lines 159-160:
```toml
[patch.crates-io]
stellar-xdr = { git = "https://github.com/stellar/rs-stellar-xdr", rev = "89cc1cbadf1b9a16843826954dede7fec514d8e7" }
```

**Why**: The patch was needed to ensure that transitive dependencies on
`stellar-xdr` (from crates.io) were redirected to the git revision. With the
workspace now using crates.io 25.0.0, this is no longer needed.

**Risk**: soroban-env-host P24 depends on `stellar-xdr = "=24.0.0"` from
crates.io. Removing the patch means P24 continues using the stock 24.0.0 from
crates.io (which is what already happens today — the patch version 24.0.1
doesn't satisfy the `=24.0.0` constraint). No behavioral change.

### Step 3: Build and fix compilation errors

```bash
cargo build --all 2>&1 | tee build-output.txt
```

Expected issues:

1. **Minor API differences**: The git 24.0.1 may have slightly different helper
   methods or trait implementations compared to the published 25.0.0. These
   would manifest as compilation errors.

2. **`serde_with` dependency**: The git 24.0.1 depends on `serde_with`, but
   25.0.0 may handle serde differently. Check if any code relies on
   `serde_with`-specific behavior.

3. **`base64` feature**: The crates.io 25.0.0 lists `base64` as a dependency
   (it's used by the soroban-env-host P25 path). Henyey doesn't request this
   feature at the workspace level, but soroban-env-host P25 does. Since types
   are now unified, this should work fine.

### Step 4: Verify P25 type unification

After successful build, verify that workspace XDR types are now the same Rust
types as soroban-env-host P25's XDR:

```rust
// This should compile without conversion:
let workspace_key: stellar_xdr::curr::LedgerKey = ...;
let p25_key: soroban_env_host_p25::xdr::LedgerKey = workspace_key; // same type!
```

If this works, the P25 XDR bridge functions can be removed:
- `convert_ledger_key_from_p25()` — just use the key directly
- `convert_ledger_entry_to_p25()` — just use the entry directly
- `convert_contract_cost_params_to_p25()` — just use the params directly
- `convert_diagnostic_events_p25()` — just use the events directly
- `xdr_encode_setup()` calls for P25 input path — eliminated
- `from_xdr()` calls in P25 output path — eliminated

### Step 5: Simplify P25 soroban code (if types unified)

**Files to modify**:

- `crates/tx/src/soroban/host.rs`:
  - Remove `type P25LedgerKey = soroban_env_host25::xdr::LedgerKey` (line 42)
  - Remove `type P25LedgerEntry = soroban_env_host25::xdr::LedgerEntry` (line 43)
  - Remove `convert_ledger_key_from_p25()` (line 717)
  - Remove `convert_ledger_entry_to_p25()` (line 724)
  - Remove `convert_contract_cost_params_to_p25()` (line 731)
  - Remove `convert_diagnostic_events_p25()` (line 2001)
  - Remove `xdr_encode_setup()` (line 1011) — no longer needed for P25 path
  - Simplify `LedgerSnapshotAdapterP25::get()` (line 584) — no conversion needed
  - Simplify `execute_host_function_p25()` — pass typed objects directly

- `crates/tx/src/soroban/error.rs`:
  - Remove P25 ScError conversion if types are unified

- `crates/ledger/src/soroban_state.rs`:
  - Remove `convert_ledger_entry_to_p25()` usage
  - Remove `convert_contract_cost_params_to_p25()` usage

- `crates/tx/src/soroban/protocol/p25.rs`:
  - Remove P25 type conversion functions

### Step 6: Run tests

```bash
cargo test --all
cargo clippy --all
```

### Step 7: Verify parity

```bash
./target/release/henyey offline verify-execution --testnet --from <START> --to <END> --stop-on-error --show-diff
```

Run on at least 1000 consecutive mainnet or testnet ledgers to verify hash parity.

## What This Enables

Once XDR types are unified between henyey workspace and soroban-env-host P25:

1. **Direct `Host` construction**: Build `Storage`, `Footprint`, `StorageMap`
   from workspace types without any conversion.

2. **Direct `invoke_function` call**: Pass workspace `HostFunction` directly.

3. **Direct result extraction**: Get typed `ScVal`, `Storage`, `Events` back
   without deserialization.

4. **Custom diffing**: Compare pre/post `Storage` maps directly against
   henyey's own ledger state, eliminating `init_storage_map.metered_clone()`
   and all the `get_ledger_changes()` re-serialization.

5. **Performance**: Estimated elimination of ~50 XDR operations per TX + deep
   storage clone. Combined with the upcoming typed API bypass, this addresses
   the full ~45μs/TX XDR overhead identified in profiling.

## Risks

1. **API differences between git 24.0.1 and crates.io 25.0.0**: The git rev
   was likely a development snapshot. There may be minor differences in:
   - New enum variants in 25.0.0 not present in 24.0.1
   - Changed variant names or field names
   - Different trait implementations
   - **Mitigation**: `cargo build --all` will surface all issues immediately.

2. **P24 bridge code changes**: The P24 bridge code (`convert_*_to_p24()`) must
   continue working. Since P24 uses `stellar-xdr 24.0.0` and the workspace
   moves from 24.0.1 to 25.0.0, the XDR byte-level compatibility is maintained
   for all types that exist in both versions. P24 code never encounters
   P25-only types, so this is safe.

3. **Compile time**: Going from 3 stellar-xdr versions to 2 should slightly
   reduce compile times.

## Estimated Effort

- **Step 1-2**: 5 minutes (Cargo.toml changes)
- **Step 3**: 30-60 minutes (fix compilation errors, likely few)
- **Step 4-5**: 2-4 hours (remove conversion code, simplify P25 path)
- **Step 6**: 15 minutes (test suite)
- **Step 7**: 30 minutes (parity verification)

**Total: 3-6 hours**
