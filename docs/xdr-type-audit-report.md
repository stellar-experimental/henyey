# XDR Type Audit Report

**Date**: 2026-03-11
**Scope**: All 15 workspace crates in `henyey-2`
**Guideline**: "Prefer using types provided by `rs-stellar-xdr` over creating new ones and adding many conversions." (AGENTS.md)

## Summary

A comprehensive audit was performed across the entire workspace to identify violations of the XDR type preference guideline. The audit covered six categories:

| Category | Description |
|----------|-------------|
| `RAW_BYTES` | `[u8; 32]` or `Vec<u8>` used where a typed XDR wrapper exists |
| `MIRROR_ENUM` | Custom enum that duplicates an XDR enum's variants |
| `WRAPPER_TYPE` | Custom struct that wraps or subsets an XDR struct |
| `DUPLICATE_HELPER` | Utility function that reimplements XDR trait functionality |
| `CONVERSION_BOILERPLATE` | Manual `to_xdr()`/`from_xdr()` that could use `From`/`Into` |
| `MANUAL_DISCRIMINANT` | Hand-rolled match on enum variants instead of `discriminant()` |

### Work completed

Two phases of changes were applied and committed:

**Phase 1 — `henyey-tx`** (commit `33307b1`): 12 findings fixed, including replacing `[u8; 32]` map keys with `AccountId`/`Hash`/`PoolId`/`ClaimableBalanceId`, replacing custom `AssetKey` with `TrustLineAsset`, replacing custom `EventType` with `ContractEventType`, consolidating `entry_to_key`/`asset_to_trustline_asset`/`asset_issuer` into `henyey-common`, and consolidating the `create_test_account_id` test helper.

**Phase 2 — All other crates** (6 commits):

| Crate | Commit | Changes |
|-------|--------|---------|
| common | `f3b95c5` | Removed duplicate `ledger_entry_key` |
| ledger | `eb18b72` | `ConfigSettingId` as map key, `Hash` for soroban state maps and seen_hashes |
| bucket | `9037bf4` | `discriminant()` calls replace hand-rolled matches, duplicate comparisons removed |
| herder | `b54b8a8` | `Hash256` for quorum set hash map key |
| overlay | `5e348cb` | `Uint256` for overlay manager API params |
| history | `924d2e0` | `Hash` for tx/network hashes in cdp |

**Phase 3 — Docs cleanup** (commit `c218714`): Moved 7 completed docs to `docs/archive/`.

---

## Remaining findings

The following items were identified during the final full-workspace audit but have **not yet been addressed**. They are ordered by estimated impact.

### Medium effort

#### 1. `common` — `ThresholdLevel` mirrors `ThresholdIndexes`

- **File**: `crates/common/src/types.rs`
- **Category**: `MIRROR_ENUM`
- **Description**: The custom `ThresholdLevel` enum (`MasterWeight`, `Low`, `Medium`, `High`) mirrors the XDR `ThresholdIndexes` enum. Approximately 64 references across the workspace.
- **Risk**: Medium — widespread usage means a rename touches many files, but it is a straightforward mechanical substitution.
- **Recommendation**: Replace `ThresholdLevel` with `stellar_xdr::curr::ThresholdIndexes` and update all call sites. The XDR type has identical semantics and derives all necessary traits.

#### 2. `bucket` — Custom `StateArchivalSettings` duplicates XDR type

- **File**: `crates/bucket/src/` (multiple files)
- **Category**: `WRAPPER_TYPE`
- **Description**: A custom `StateArchivalSettings` struct holds a subset of fields from the XDR `StateArchivalSettings`. This creates conversion boilerplate and a maintenance burden when fields change.
- **Recommendation**: Replace with the XDR `StateArchivalSettings` directly, accessing only the needed fields at point of use.

#### 3. `overlay` — Custom `AuthCert` duplicates XDR `AuthCert`

- **Files**: `crates/overlay/src/auth.rs`, `crates/overlay/src/manager.rs`
- **Category**: `WRAPPER_TYPE` + `CONVERSION_BOILERPLATE`
- **Description**: A custom `AuthCert` struct stores raw `[u8; 32]` and `[u8; 64]` fields that map directly to XDR `AuthCert` fields (`Curve25519Public`, `Signature`). Includes `to_xdr()`/`from_xdr()` conversion methods.
- **Recommendation**: Replace with XDR `AuthCert` directly. The raw byte fields should become their typed XDR equivalents.

### Low effort

#### 4. `tx` — `ApplyContext.network_id` uses `[u8; 32]`

- **File**: `crates/tx/src/apply.rs`
- **Category**: `RAW_BYTES`
- **Description**: `ApplyContext.network_id` is `[u8; 32]` but should be `Hash` (the XDR type for network IDs). This is the last remaining `[u8; 32]` in `henyey-tx`'s non-crypto public API.
- **Recommendation**: Change to `Hash` and update call sites.

#### 5. `tx` — `OpResultCode` mirrors XDR discriminant

- **File**: `crates/tx/src/operations/mod.rs`
- **Category**: `MIRROR_ENUM`
- **Description**: `OpResultCode` provides a compact error code enum that mirrors the discriminant values from `OperationResult`. Could potentially use the XDR type's `discriminant()` method instead.
- **Recommendation**: Evaluate whether `OperationResult` discriminants can replace this enum. May be intentional for ergonomic error handling — investigate before changing.

#### 6. `tx` — `ContractEvent` wrapper (test-only)

- **File**: `crates/tx/src/soroban/events.rs`
- **Category**: `WRAPPER_TYPE`
- **Description**: A `ContractEvent` struct wraps the XDR `ContractEvent` with added convenience methods. Used only in tests.
- **Recommendation**: Low priority. If the wrapper only adds test convenience, consider moving it to a `#[cfg(test)]` module or replacing with extension traits.

#### 7. `ledger` — `delta::entry_to_key` wraps infallible function in `Result`

- **File**: `crates/ledger/src/delta.rs`
- **Category**: `DUPLICATE_HELPER`
- **Description**: `delta::entry_to_key()` wraps the infallible `henyey_common::entry_to_key()` in an unnecessary `Result`, adding error handling boilerplate at every call site.
- **Recommendation**: Call `henyey_common::entry_to_key()` directly and remove the wrapper.

#### 8. `common` — `entry_to_key` re-wrapped in tx and ledger

- **Files**: `crates/tx/src/lib.rs`, `crates/ledger/src/lib.rs`
- **Category**: `DUPLICATE_HELPER`
- **Description**: Both crates re-export or thin-wrap `henyey_common::entry_to_key()`. Call sites within those crates could use the common function directly.
- **Recommendation**: Remove the wrappers and import `henyey_common::entry_to_key` at call sites.

#### 9. `common` — `asset_to_trustline_asset` re-wrapped in 3 locations

- **Files**: `crates/tx/src/`, `crates/ledger/src/`, `crates/bucket/src/`
- **Category**: `DUPLICATE_HELPER`
- **Description**: Thin re-exports of `henyey_common::asset::asset_to_trustline_asset()`.
- **Recommendation**: Remove wrappers, import directly from `henyey_common::asset`.

#### 10. `bucket` — `entry_type_to_u32` / `u32_to_entry_type` duplicate XDR `From`/`TryFrom`

- **File**: `crates/bucket/src/entry.rs`
- **Category**: `DUPLICATE_HELPER`
- **Description**: Manual conversion functions between `LedgerEntryType` and `u32` that duplicate the XDR type's built-in `From<LedgerEntryType> for i32` and `TryFrom<i32> for LedgerEntryType`.
- **Recommendation**: Replace with `i32::from(entry_type) as u32` and `LedgerEntryType::try_from(val as i32)`.

---

## Clean crates (no findings)

The following crates had no XDR type preference violations:

- `crypto` — Correctly uses raw bytes for cryptographic operations
- `app` — Thin orchestration layer, delegates to typed crates
- `herder` — Clean after Phase 2 fix
- `scp` — Protocol logic uses XDR types throughout
- `history` — Clean after Phase 2 fix
- `historywork` — Uses XDR types correctly
- `simulation` — Uses XDR types correctly
- `henyey` — Binary crate, minimal type usage
- `db` — Storage layer, no XDR type issues
- `clock` — Timer utilities, no XDR types needed
- `work` — Task scheduling, no XDR types needed

---

## Key discoveries

1. **XDR types are HashMap-safe**: All relevant XDR types (`AccountId`, `Hash`, `PoolId`, `ClaimableBalanceId`, `TrustLineAsset`, `ConfigSettingId`, etc.) derive `Clone, Hash, PartialEq, Eq, PartialOrd, Ord` — fully valid as `HashMap`/`HashSet` keys.

2. **`AccountId`/`Hash` are Clone, not Copy**: When replacing `[u8; 32]` (which is `Copy`) with XDR types (which are only `Clone`), explicit `.clone()` calls are needed at entry/lookup sites.

3. **XDR `discriminant()` method**: XDR enums provide a built-in `discriminant()` method returning the `#[repr(i32)]` value, eliminating the need for hand-rolled match arms.

4. **`Hash256` is legitimate**: The `Hash256` utility type in `henyey-common` provides `hash()`, `hash_xdr()`, and hex conversions. It is a value-add utility, not an XDR violation.

5. **Crypto operations are exempt**: Functions like `verify_signature_with_raw_key` intentionally use `[u8; 32]` for raw cryptographic key material — this is correct and should not be changed.
