# XDR Type Redundancy Audit

**Date**: 2026-03-10
**XDR Crate**: `stellar-xdr = "=25.0.0"` (feature: `curr`)
**Scope**: All crates under `crates/`

---

## Executive Summary

This audit identifies custom types, enums, structs, constants, and type aliases across
the henyey codebase that are redundant with types already defined in the `stellar_xdr`
crate. The goal is to reduce conversion boilerplate, eliminate duplicate definitions,
and improve type safety by using XDR types directly where possible.

### Implementation Status

All Tier 1 and Tier 2 high-priority items have been implemented. Build, clippy, and
tests pass clean (excluding a pre-existing compile error in `preconditions.rs:631`).

| Item | Description | Status |
|------|-------------|--------|
| **Tier 1** | | |
| T1.1 | Replace `EventType` with `ContractEventType` | **Done** |
| T1.2 | Replace `AUTH_*_FLAG` constants with `AccountFlags::* as u32` | **Done** |
| T1.3 | Replace local `MAX_SIGNERS`, `MASK_LEDGER_HEADER_FLAGS`, `MAX_OPS_PER_TX` | **Done** |
| T1.4 | Consolidate `OfferDescriptor` | **Done** |
| T1.5 | Consolidate `ThresholdLevel` to `henyey_common` | **Done** |
| T1.6 | Replace BN254 cost type index constants | **Done** |
| T1.7 | Replace hardcoded `fee: 30` with `LIQUIDITY_POOL_FEE_V18` | **Done** |
| T1.8 | Consolidate `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION` | **Done** |
| T1.9 | Consolidate `TRUSTLINE_CLAWBACK_ENABLED_FLAG` | **Done** |
| **Tier 2** | | |
| T2.1 | Merge `StorageKey` and `ContractDataKey` | Deferred |
| T2.2 | Replace `OperationType` with XDR `OperationType` | **Done** |
| T2.3 | Replace `ExecutionFailure` with `TransactionResultCode` | **Done** |
| T2.4 | Replace `TxResultCode` with `TransactionResultCode` | **Done** |
| T2.5 | Merge `AssetKey` definitions | Deferred |
| T2.6 | Replace `EvictionIterator` with XDR `EvictionIterator` | **Done** |
| T2.7 | Replace `BucketEntry` with XDR `BucketEntry` | **Done** |
| T2.8 | Use `protocol_version_starts_from()` consistently | Deferred |
| **Tier 3** | | |
| T3.1 | Evaluate `Hash256` -> `xdr::Hash` migration | Not started |
| T3.2 | Evaluate `PeerId` -> `NodeId` migration | Not started |
| T3.3 | Introduce semantic type aliases | Not started |
| T3.4 | Evaluate custom `ContractEvent` -> XDR `ContractEvent` | Not started |

**Pattern used**: For each replacement, the custom type becomes a type alias
(`pub type Foo = stellar_xdr::curr::Foo`) and custom methods move to an extension
trait (`FooExt`). This preserves API ergonomics while eliminating conversion boilerplate.

### Key Findings

| Category | High Redundancy | Medium | Low / Justified |
|----------|:-:|:-:|:-:|
| Enums mirroring XDR enums | 5 | 2 | 3 |
| Structs duplicating XDR structs | 4 | 3 | 6 |
| Type aliases / newtypes | 2 | 3 | 4 |
| Constants duplicating XDR constants | 6 groups | 4 groups | 5 groups |
| Internal duplications (non-XDR) | 4 pairs | -- | -- |

**Estimated conversion call sites that could be eliminated**: ~200+

---

## Part 1: XDR Type Manifest

The project references **~228 distinct XDR types** from `stellar_xdr::curr`. The top 25
most-referenced types (by file count) are:

| Rank | Type | Files | Rank | Type | Files |
|------|------|:-----:|------|------|:-----:|
| 1 | `Hash` | 130 | 14 | `LedgerEntryData` | 58 |
| 2 | `Uint256` | 127 | 15 | `AccountEntryExt` | 57 |
| 3 | `PublicKey` | 119 | 16 | `Asset` | 56 |
| 4 | `Limits` | 98 | 17 | `Thresholds` | 55 |
| 5 | `AccountId` | 89 | 18 | `String32` | 54 |
| 6 | `Transaction` | 82 | 19 | `LedgerEntryExt` | 48 |
| 7 | `WriteXdr` | 82 | 20 | `LedgerKeyAccount` | 46 |
| 8 | `LedgerKey` | 77 | 21 | `TransactionEnvelope` | 42 |
| 9 | `SequenceNumber` | 75 | 22 | `Operation` | 39 |
| 10 | `AccountEntry` | 60 | 23 | `NodeId` | 39 |
| 11 | `Duration` | 59 | 24 | `ScVal` | 38 |
| 12 | `VecM` | 59 | 25 | `Signature` | 37 |
| 13 | `LedgerEntry` | 58 | | | |

Full breakdown by domain: ~134 structs, ~126 enums/unions, 2 traits (`ReadXdr`, `WriteXdr`),
4 generic containers (`VecM`, `BytesM`, `StringM`, `Limited`).

---

## Part 2: Redundant Enum Definitions

### HIGH redundancy -- direct replacements recommended

#### 2.1 `OperationType` (tx crate)

- **Location**: `crates/tx/src/operations/mod.rs:23`
- **XDR equivalent**: `stellar_xdr::curr::OperationType`
- **Evidence**: Identical variant names. Test code explicitly shadows the XDR type:
  `use stellar_xdr::curr::*; // Re-import to shadow XDR's OperationType`
- **Conversion**: `from_body()` method at line 73 maps `OperationBody` -> custom `OperationType`
- **Recommendation**: Replace with `stellar_xdr::curr::OperationType` directly. The `from_body()` helper
  can be a free function returning the XDR type.

#### 2.2 `TxResultCode` (tx crate)

- **Location**: `crates/tx/src/result.rs:249`
- **XDR equivalent**: `stellar_xdr::curr::TransactionResultCode`
- **Evidence**: 19 variants with identical names (e.g., `TxSuccess`, `TxFailed`, `TxBadSeq`)
- **Conversion**: `to_xdr_result()` at line 285 provides 1:1 mapping
- **Recommendation**: Replace with `TransactionResultCode`. The `name()` method can be implemented
  as a trait extension.

#### 2.3 `OpResultCode` (tx crate)

- **Location**: `crates/tx/src/result.rs:500`
- **XDR equivalent**: `stellar_xdr::curr::OperationResult` discriminant
- **Evidence**: 7 variants matching XDR union discriminants exactly
- **Conversion**: `result_code()` at line 485 maps 1:1
- **Recommendation**: Replace with direct discriminant matching on `OperationResult`.

#### 2.4 `ExecutionFailure` (ledger crate)

- **Location**: `crates/ledger/src/execution/mod.rs:333`
- **XDR equivalent**: `stellar_xdr::curr::TransactionResultCode`
- **Evidence**: 15 variants, each with a 1:1 XDR correspondent (shortened names)
- **Conversion**: `map_failure_to_result()` and `map_failure_to_inner_result()` in
  `crates/ledger/src/execution/result_mapping.rs:8-76`
- **Note**: This duplicates the same XDR concept as `TxResultCode` above -- two independent
  re-creations of `TransactionResultCode` in different crates.
- **Recommendation**: Replace both `ExecutionFailure` and `TxResultCode` with
  `TransactionResultCode`.

#### 2.5 `EventType` (tx crate)

- **Location**: `crates/tx/src/soroban/events.rs:9`
- **XDR equivalent**: `stellar_xdr::curr::ContractEventType`
- **Evidence**: Identical variants: `Contract`, `System`, `Diagnostic`
- **Conversion**: `event_to_xdr()` at line 174 maps `EventType::Contract => ContractEventType::Contract`, etc.
- **Recommendation**: Drop `EventType` entirely; use `ContractEventType` directly.

### MEDIUM redundancy

#### 2.6 `BucketEntry` (bucket crate)

- **Location**: `crates/bucket/src/entry.rs:75`
- **XDR equivalent**: `stellar_xdr::curr::BucketEntry` (imported as `XdrBucketEntry`)
- **Evidence**: Same 4 variants carrying the same XDR inner types. Only difference is naming
  (`Live` vs `Liveentry`, `Dead` vs `Deadentry`).
- **Conversion**: Bidirectional `from_xdr_entry()`/`to_xdr_entry()` at lines 99-116
- **Recommendation**: Consider using XDR `BucketEntry` directly with accessor methods as trait
  extensions. The ergonomic renaming is minor.

#### 2.7 `AssetKey` (tx crate -- TWO definitions)

- **Location 1**: `crates/tx/src/apply.rs:595` -- variants: `Native`, `CreditAlphanum4`, `CreditAlphanum12`
- **Location 2**: `crates/tx/src/state/mod.rs:300` -- same + `PoolShare`
- **XDR equivalent**: `stellar_xdr::curr::Asset` (and `TrustLineAsset` for the PoolShare variant)
- **Evidence**: Both have `from_asset()` methods converting from XDR `Asset`
- **Note**: These exist because they use raw `[u8; N]` arrays for `Hash`/`Eq`-ability. However,
  XDR `Asset` already derives `Hash`, `Eq`, and `Ord`.
- **Recommendation**: Consolidate into one type. Investigate whether XDR `Asset` / `TrustLineAsset`
  can serve as HashMap keys directly (they derive `Hash` + `Eq`).

### LOW redundancy / internal duplications

#### 2.8 `ThresholdLevel` -- duplicated in two crates

- `crates/tx/src/operations/mod.rs:568`
- `crates/ledger/src/execution/mod.rs:3792`
- Not an XDR type, but identical enum + helper functions in both locations.
- **Recommendation**: Move to `henyey_common`.

#### 2.9 `SurveyPhase` -- duplicated in two crates

- `crates/overlay/src/survey.rs:63`
- `crates/app/src/survey.rs:59`
- Identical enum in both locations.
- **Recommendation**: Move to shared module.

#### 2.10 `ValueValidation` vs `scp::ValidationLevel`

- `crates/herder/src/scp_driver.rs:66`
- Mirrors `scp::driver::ValidationLevel` with 1:1 conversion at line 2069.
- **Recommendation**: Use `ValidationLevel` directly.

#### 2.11 `EntryCountType` (bucket crate)

- `crates/bucket/src/metrics.rs:146`
- Mirrors `BucketEntryType` with shortened names. Used only for metrics.
- **Recommendation**: Low priority; could use `BucketEntryType` with display formatting.

---

## Part 3: Redundant Struct Definitions

### HIGH redundancy

#### 3.1 `StorageKey` (tx crate)

- **Location**: `crates/tx/src/soroban/storage.rs:13`
- **XDR equivalent**: `stellar_xdr::curr::LedgerKeyContractData`
- **Fields**: `contract: ScAddress`, `key: ScVal`, `durability: ContractDataDurability`
  -- identical to `LedgerKeyContractData`
- **Reason for existence**: `Hash + Eq` derives for HashMap key usage
- **Recommendation**: Check if `LedgerKeyContractData` can derive `Hash`. If not, implement
  `Hash` via a newtype or extension. Eliminates a redundant type and its `to_ledger_key()` method.

#### 3.2 `ContractDataKey` (tx crate) -- also duplicates `StorageKey`

- **Location**: `crates/tx/src/state/mod.rs:313`
- **XDR equivalent**: Same as above -- `LedgerKeyContractData`
- **Note**: This is a second copy of `StorageKey` in the same crate.
- **Recommendation**: Consolidate `StorageKey` and `ContractDataKey` into one type, ideally
  the XDR type itself.

#### 3.3 `ContractEvent` (custom, tx crate)

- **Location**: `crates/tx/src/soroban/events.rs:20`
- **XDR equivalent**: `stellar_xdr::curr::ContractEvent`
- **Fields**: `event_type`, `contract_id`, `topics`, `data` -- maps closely to XDR's
  `type_`, `contract_id`, `body.v0().topics`, `body.v0().data`
- **Recommendation**: Use XDR `ContractEvent` directly. Factory methods can be free functions.

#### 3.4 `EvictionIterator` (bucket crate)

- **Location**: `crates/bucket/src/eviction.rs` (definition)
- **XDR equivalent**: `stellar_xdr::curr::EvictionIterator`
- **Fields**: Identical -- `bucket_file_offset: u64`, `bucket_list_level: u32`, `is_curr_bucket: bool`
- **Conversions**: Manual field-by-field copying in `ledger/src/manager.rs:852,4245` and
  `history/src/replay.rs:970`
- **Recommendation**: Use XDR type directly. Add scan methods as trait extensions.

### MEDIUM redundancy

#### 3.5 `AuthCert` (overlay crate)

- **Location**: `crates/overlay/src/auth.rs`
- **XDR equivalent**: `stellar_xdr::curr::AuthCert`
- **Fields**: `pubkey: [u8; 32]`, `expiration: u64`, `sig: [u8; 64]` -- maps to XDR's
  `Curve25519Public`, `u64`, `Signature`
- **Recommendation**: Could use XDR type with crypto methods as trait extensions.

#### 3.6 `LedgerInfo` (ledger crate)

- **Location**: `crates/ledger/src/lib.rs:134`
- **XDR equivalent**: Subset of `stellar_xdr::curr::LedgerHeader`
- **Conversion**: `From<&LedgerHeader> for LedgerInfo` (one-way, lossy projection)
- **Recommendation**: Consider replacing with a reference to `LedgerHeader` or a trait
  providing the subset of fields.

#### 3.7 `StorageEntry` (tx crate)

- **Location**: `crates/tx/src/soroban/storage.rs:56`
- **XDR equivalent**: `ContractDataEntry` + `TtlEntry` merged
- **Note**: Bundles data from two XDR types. Not directly replaceable but could be refactored.

### LOW redundancy (justified wrappers)

These wrappers add genuine functionality beyond the XDR types:

| Struct | Location | Wraps | Justification |
|--------|----------|-------|---------------|
| `TransactionFrame` | `tx/src/frame.rs:69` | `TransactionEnvelope` | Unified API over V0/V1/FeeBump variants |
| `TxResultWrapper` | `tx/src/result.rs:64` | `TransactionResult` | Typed accessors for deeply nested union |
| `OpResultWrapper` | `tx/src/result.rs:351` | `OperationResult` | Same |
| `MutableTransactionResult` | `tx/src/result.rs:740` | `TransactionResult` | Builder pattern + fee tracking |
| `Hash256` | `common/src/types.rs:33` | `[u8; 32]` (same as `Hash`) | Utility methods (hex, display, hashing) |
| `PublicKey` / `SecretKey` | `crypto/src/keys.rs` | ed25519 keys | Actual crypto operations |

### Internal struct duplications

| Struct | Location 1 | Location 2 | Recommendation |
|--------|-----------|-----------|----------------|
| `OfferDescriptor` | `ledger/src/offer.rs:47` | `tx/src/state/offer_index.rs:8` | Consolidate to one shared location |
| `StorageKey` / `ContractDataKey` | `tx/src/soroban/storage.rs:13` | `tx/src/state/mod.rs:313` | Merge within tx crate |

---

## Part 4: Type Aliases and Newtypes

### Significant findings

#### 4.1 `Hash256` -- pervasive custom hash type

- **Location**: `crates/common/src/types.rs:33`
- **Definition**: `pub struct Hash256(pub [u8; 32])`
- **XDR equivalent**: `stellar_xdr::curr::Hash([u8; 32])`
- **Usage**: ~1,289 matches across the entire codebase
- **Conversions**: Bidirectional `From` impls (lines 141-151)
- **Impact**: Every XDR boundary requires `.into()` conversion. This is by far the largest
  source of conversion boilerplate.
- **Recommendation**: Long-term, consider using `stellar_xdr::curr::Hash` directly with utility
  methods added via an extension trait. Short-term, this is too pervasive to change quickly.

#### 4.2 `PeerId` vs XDR `NodeId`

- **Location**: `crates/overlay/src/lib.rs:450`
- **Definition**: `pub struct PeerId(pub stellar_xdr::curr::PublicKey)`
- **XDR equivalent**: `stellar_xdr::curr::NodeId(PublicKey)` -- structurally identical
- **Usage**: ~169 matches in overlay crate
- **Recommendation**: Consider using `NodeId` directly with display helpers as trait extensions.

#### 4.3 Raw byte tuple keys instead of XDR ledger keys

| Alias | Location | XDR Equivalent |
|-------|----------|----------------|
| `TrustlineKey = ([u8; 32], AssetKey)` | `tx/src/state/mod.rs:36` | `LedgerKeyTrustLine` |
| `DataKey = ([u8; 32], String)` | `tx/src/state/mod.rs:38` | `LedgerKeyData` |

These decompose XDR types into raw bytes for HashMap key usage. The underlying issue is
whether XDR key types implement `Hash`.

#### 4.4 Bare primitives for semantic values

The codebase uses bare `u32`/`u64` extensively where semantic XDR types exist:

| Usage | Occurrences | XDR Type Available |
|-------|:-----------:|-------------------|
| `ledger_seq: u32` | ~174 | `Uint32` (or a `LedgerSeq` alias) |
| `close_time: u64` | ~53 | `TimePoint` |
| `protocol_version: u32` | ~123 | `Uint32` |
| `base_fee: u32` | ~55 | `Uint32` |
| `base_reserve: u32` | ~55 | `Uint32` |

- **Recommendation**: Consider defining semantic type aliases (e.g., `type LedgerSeq = u32`)
  in `henyey_common` even if not using XDR wrapper types, to improve code readability.

#### 4.5 `ApplyContext.network_id: [u8; 32]`

- **Location**: `crates/tx/src/apply.rs:400`
- Uses bare `[u8; 32]` instead of `NetworkId` or `stellar_xdr::curr::Hash`
- **Recommendation**: Use `NetworkId` for consistency.

---

## Part 5: Redundant Constants

### HIGH priority -- production code using raw values where XDR constants exist

#### 5.1 `AccountFlags` redefined as raw hex constants

The XDR `AccountFlags` enum defines `RequiredFlag = 1`, `RevocableFlag = 2`,
`ImmutableFlag = 4`, `ClawbackEnabledFlag = 8`. These are independently redefined in
**at least 6 production files** and 9 test files:

| Constant | Defined In (production) |
|----------|------------------------|
| `AUTH_REQUIRED_FLAG: u32 = 0x1` | `set_options.rs:97`, `change_trust.rs:268`, `liquidity_pool.rs:399` |
| `AUTH_REVOCABLE_FLAG: u32 = 0x2` | `set_options.rs:98`, `trust_flags.rs:26` |
| `AUTH_IMMUTABLE_FLAG: u32 = 0x4` | `set_options.rs:99`, `account_merge.rs:39` |
| `AUTH_CLAWBACK_FLAG: u32 = 0x8` | `set_options.rs:100`, `change_trust.rs:269` |

**Note**: Some files (e.g., `claimable_balance.rs:207`) already correctly use
`AccountFlags::ClawbackEnabledFlag as u32`, proving the pattern works.

**Recommendation**: Replace all `AUTH_*_FLAG` constants with `AccountFlags::* as u32`.

#### 5.2 BN254 cost type indices (15 hardcoded constants)

- **Location**: `crates/ledger/src/manager.rs:2938-2952`
- Constants like `const BN254_ENCODE_FP: usize = 70` duplicate `ContractCostType::Bn254EncodeFp as usize`
- **Recommendation**: Use `ContractCostType` enum variants directly.

#### 5.3 `TRUSTLINE_CLAWBACK_ENABLED_FLAG` defined 4 times

- `trust_flags.rs:28`, `change_trust.rs:270`, `claimable_balance.rs:391`, `clawback.rs:26`
- All derive from `TrustLineFlags::TrustlineClawbackEnabledFlag as u32`
- **Recommendation**: Define once in `crates/tx/src/operations/execute/mod.rs`.

### MEDIUM priority

#### 5.4 XDR constants redefined locally

| Local Constant | Location | XDR Constant |
|----------------|----------|-------------|
| `MAX_SIGNERS: usize = 20` | `set_options.rs:18` | `stellar_xdr::curr::MAX_SIGNERS` |
| `MASK_LEDGER_HEADER_FLAGS: u32 = 0x7` | `scp_driver.rs:1035`, `upgrades.rs:535` | `stellar_xdr::curr::MASK_LEDGER_HEADER_FLAGS` |
| `LIQUIDITY_POOL_FEE_V18: i32 = 30` | `common/asset.rs:25` | `stellar_xdr::curr::LIQUIDITY_POOL_FEE_V18` |
| `MAX_OPS_PER_TX: usize = 100` | `simulation/applyload.rs:42` | `stellar_xdr::curr::MAX_OPS_PER_TX` |

#### 5.5 Protocol version constants duplicated across crates

| Constant | Locations | Should Use |
|----------|-----------|-----------|
| `SOROBAN_PROTOCOL_VERSION: u32 = 20` | `herder/flow_control.rs:61` | `henyey_common::MIN_SOROBAN_PROTOCOL_VERSION` |
| `CURRENT_PROTOCOL_VERSION: u32 = 25` | `history/catchup.rs:80`, `overlay/lib.rs:579` | `henyey_common::CURRENT_LEDGER_PROTOCOL_VERSION` |
| `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION: u32 = 23` | `bucket/lib.rs:274`, `ledger/manager.rs:173`, `history/replay.rs:650` | Single canonical location |

#### 5.6 Hardcoded `fee: 30` in production code

- `crates/ledger/src/execution/mod.rs:1066` -- should use `LIQUIDITY_POOL_FEE_V18`
- Also appears in ~8 test files

### LOW priority -- style improvements

#### 5.7 Raw protocol version comparisons

~15 locations use `protocol_version >= 25` instead of the type-safe
`protocol_version_starts_from(v, ProtocolVersion::V25)` helper already available in
`henyey_common::protocol`.

Examples:
- `tx/src/soroban/host.rs:224,958`
- `ledger/src/soroban_state.rs:795,976`
- `ledger/src/execution/result_mapping.rs:110`
- `herder/src/scp_driver.rs:1036,2155`
- `herder/src/upgrades.rs:536,540,544`
- `ledger/src/manager.rs:3507-3550`

---

## Part 6: Conversion Layer Analysis

The codebase has **23 formal conversion implementations** (`From`/`TryFrom`/`to_xdr`/`from_xdr`)
between custom types and XDR types.

### Strongest redundancy signals (1:1 wrapping)

| Custom Type | XDR Type | Direction | Lossless? | File |
|-------------|----------|-----------|:---------:|------|
| `BucketEntry` | `xdr::BucketEntry` | Bidirectional | Yes | `bucket/entry.rs` |
| `TxResultWrapper` | `TransactionResult` | Bidirectional | Yes | `tx/result.rs` |
| `OpResultWrapper` | `OperationResult` | Bidirectional | Yes | `tx/result.rs` |
| `Hash256` | `xdr::Hash` | Bidirectional | Yes | `common/types.rs` |
| `Curve25519Public` | `xdr::Curve25519Public` | Bidirectional | Yes | `crypto/curve25519.rs` |
| `Curve25519Secret` | `xdr::Curve25519Secret` | Bidirectional | Yes | `crypto/curve25519.rs` |
| `EvictionIterator` | `xdr::EvictionIterator` | Bidirectional | Yes | `bucket/eviction.rs` + `ledger/manager.rs` |
| `AuthCert` | `xdr::AuthCert` | Bidirectional | Yes | `overlay/auth.rs` |

### Moderate redundancy (adds some state or transforms structure)

| Custom Type | XDR Type | Notes |
|-------------|----------|-------|
| `MutableTransactionResult` | `TransactionResult` | Adds `refundable_fee_tracker` |
| `FeeBumpMutableTransactionResult` | `TransactionResult` | Decomposes nested inner result |
| `TransactionSet` (herder) | `StoredTransactionSet` | Adds precomputed hash |
| `ContractEvent` (custom) | `xdr::ContractEvent` | Flattens nested body structure |
| `TxResultCode` | `TransactionResultCode` | Tag-only view of union |

### Justified (different representation or adds crypto)

| Custom Type | XDR Type | Justification |
|-------------|----------|---------------|
| `PublicKey` (crypto) | `xdr::PublicKey` | Wraps ed25519 `VerifyingKey`, adds verification |
| `Signature` (crypto) | `xdr::Signature` | Fixed 64-byte invariant vs variable-length |
| `NetworkId` | `xdr::Hash` | Semantic type + factory methods |
| `TransactionFrame` | `TransactionEnvelope` | Unified API over 3 envelope variants |
| `ConfigUpgradeSetFrame` | `ConfigUpgradeSet` | Adds validation state |
| `PeerAddress` (custom) | `xdr::PeerAddress` | String hostnames vs binary IPs |

---

## Part 7: Prioritized Recommendations

### Tier 1 -- Quick wins (low risk, high value)

1. **Replace `EventType` with `ContractEventType`** -- 3-variant enum, exact duplicate
2. **Replace `AUTH_*_FLAG` constants with `AccountFlags::* as u32`** -- already proven in
   `claimable_balance.rs`
3. **Replace local `MAX_SIGNERS`, `MASK_LEDGER_HEADER_FLAGS`, `MAX_OPS_PER_TX`** with XDR constants
4. **Consolidate `OfferDescriptor`** into one shared location
5. **Consolidate `ThresholdLevel`** into one shared location
6. **Replace BN254 cost type index constants** with `ContractCostType` enum variants
7. **Replace hardcoded `fee: 30`** with `LIQUIDITY_POOL_FEE_V18` constant
8. **Consolidate `FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION`** to single definition

### Tier 2 -- Moderate refactors (medium risk)

9. **Merge `StorageKey` and `ContractDataKey`** into one type (or XDR `LedgerKeyContractData`
   if it supports `Hash + Eq`)
10. **Replace `OperationType`** with XDR `OperationType`
11. **Replace `ExecutionFailure`** with `TransactionResultCode`
12. **Replace `TxResultCode`** with `TransactionResultCode`
13. **Merge `AssetKey` definitions** -- investigate whether XDR `Asset` works as HashMap key
14. **Replace `EvictionIterator`** with XDR `EvictionIterator`
15. **Replace `BucketEntry`** with XDR `BucketEntry` + extension trait for convenience methods
16. **Use `protocol_version_starts_from()`** helpers consistently

### Tier 3 -- Large refactors (high risk, long-term)

17. **Evaluate `Hash256` -> `xdr::Hash` migration** -- ~1,289 call sites. Would need extension
    trait for `hash()`, `from_hex()`, `to_hex()`, `is_zero()`, `ZERO`, `Display`.
    This is the single largest source of conversion boilerplate.
18. **Evaluate `PeerId` -> `NodeId` migration** -- ~169 call sites in overlay crate
19. **Introduce semantic type aliases** for `u32` ledger sequence / `u64` close time
20. **Evaluate custom `ContractEvent` -> XDR `ContractEvent`**

### Not recommended for change

- `TransactionFrame` -- justified unified API wrapper
- `PublicKey` / `SecretKey` / `Signature` (crypto crate) -- provide actual crypto operations
- `MutableTransactionResult` -- justified builder pattern
- `TxResultWrapper` / `OpResultWrapper` -- justified accessor wrappers
- `NetworkId` -- justified semantic newtype
- P24 <-> P25 conversion shims -- necessary for cross-version compatibility

---

## Appendix A: Internal Duplications (Non-XDR)

These are custom types that are duplicated within the codebase itself:

| Type | Location 1 | Location 2 | Fields |
|------|-----------|-----------|--------|
| `OfferDescriptor` | `ledger/src/offer.rs:47` | `tx/src/state/offer_index.rs:8` | `price: Price, offer_id: i64` |
| `StorageKey` / `ContractDataKey` | `tx/src/soroban/storage.rs:13` | `tx/src/state/mod.rs:313` | `contract, key, durability` |
| `ThresholdLevel` | `tx/src/operations/mod.rs:568` | `ledger/src/execution/mod.rs:3792` | `Low, Medium, High` |
| `SurveyPhase` | `overlay/src/survey.rs:63` | `app/src/survey.rs:59` | `Collecting, Reporting, Inactive` |
| `AssetKey` | `tx/src/apply.rs:595` | `tx/src/state/mod.rs:300` | `Native, CreditAlphanum4, CreditAlphanum12` |

---

## Appendix B: XDR Constants Available but Not Used

| XDR Constant | Value | Currently Redefined As |
|-------------|:-----:|----------------------|
| `stellar_xdr::curr::MAX_SIGNERS` | 20 | `const MAX_SIGNERS` in 2 files |
| `stellar_xdr::curr::MASK_LEDGER_HEADER_FLAGS` | 0x7 | `const MASK_LEDGER_HEADER_FLAGS` in 2 files |
| `stellar_xdr::curr::LIQUIDITY_POOL_FEE_V18` | 30 | `const LIQUIDITY_POOL_FEE_V18` in 1 file + magic `30` in ~9 files |
| `stellar_xdr::curr::MAX_OPS_PER_TX` | 100 | `const MAX_OPS_PER_TX` in 1 file |
| `AccountFlags::RequiredFlag` | 0x1 | `AUTH_REQUIRED_FLAG` in ~6 prod files |
| `AccountFlags::RevocableFlag` | 0x2 | `AUTH_REVOCABLE_FLAG` in ~3 files |
| `AccountFlags::ImmutableFlag` | 0x4 | `AUTH_IMMUTABLE_FLAG` in ~2 files |
| `AccountFlags::ClawbackEnabledFlag` | 0x8 | `AUTH_CLAWBACK_FLAG` in ~3 files |

---

## Appendix C: Files With Most XDR Conversion Boilerplate

| File | Conversions | Primary Types |
|------|:-----------:|--------------|
| `crates/tx/src/result.rs` | 12 | `TransactionResult`, `OperationResult`, result codes |
| `crates/bucket/src/entry.rs` | 4 | `BucketEntry` |
| `crates/crypto/src/keys.rs` | 5 | `PublicKey`, `Signature` |
| `crates/crypto/src/curve25519.rs` | 4 | `Curve25519Public`, `Curve25519Secret` |
| `crates/overlay/src/auth.rs` | 2 | `AuthCert` |
| `crates/tx/src/soroban/events.rs` | 3 | `ContractEvent`, `EventType` |
| `crates/ledger/src/manager.rs` | 4 | `EvictionIterator`, cost params |
| `crates/herder/src/tx_queue/tx_set.rs` | 2 | `TransactionSet` |
| `crates/tx/src/soroban/host.rs` | 14 | P24<->P25 cross-version shims |
