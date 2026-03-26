# stellar-core Parity Status

**Crate**: `henyey-common`
**Upstream**: `stellar-core/src/util/`
**Overall Parity**: 90%
**Last Updated**: 2026-03-25

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Protocol version helpers | Partial | Missing `protocolVersionEquals()` and `V_26` |
| Hash and ledger key utilities | Full | Hash xor, zero-check, key extraction implemented |
| Asset and balance helpers | Partial | `getIssuer`/`isIssuer` not fully generic |
| Numeric helpers | Full | 64-bit and 128-bit helpers match mapped APIs |
| Resource accounting | Partial | `anyLessThan()` and `limitTo()` missing |
| Metadata normalization | Partial | `TransactionMeta` only |
| XDR frame streams | Partial | `readPage()` missing |
| Durable filesystem rename | Full | Crash-safe rename implemented |
| Rust-only support modules | Full | Config, network, time, memory have no direct util peer |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `ProtocolVersion.h` / `ProtocolVersion.cpp` | `protocol.rs` | Mostly complete; missing equality helper and `V_26` |
| `types.h` / `types.cpp` | `types.rs` | Hash helpers and `LedgerEntryKey()` |
| `types.h` / `types.cpp` | `asset.rs` | Asset validation, issuer helpers, bucket key helpers, balance math |
| `numeric.h` / `numeric.cpp` | `math.rs` | Full mapped arithmetic parity |
| `numeric128.h` / `numeric128.cpp` | `math.rs` | Full mapped 128-bit helpers; `hugeDivide()` omitted |
| `TxResource.h` / `TxResource.cpp` | `resource.rs` | Core arithmetic implemented; two comparison helpers missing |
| `MetaUtils.h` / `MetaUtils.cpp` | `meta.rs` | `TransactionMeta` normalization only |
| `XDRStream.h` | `xdr_stream.rs` | Frame I/O implemented; page scanning missing |
| `Fs.h` / `Fs.cpp` | `fs_utils.rs` | Only `durableRename()` is ported here |

## Component Mapping

### protocol (`protocol.rs`)

Corresponds to: `ProtocolVersion.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ProtocolVersion` enum | `ProtocolVersion` enum | Partial |
| `protocolVersionIsBefore()` | `protocol_version_is_before()` | Full |
| `protocolVersionStartsFrom()` | `protocol_version_starts_from()` | Full |
| `protocolVersionEquals()` | — | None |
| `SOROBAN_PROTOCOL_VERSION` | `SOROBAN_PROTOCOL_VERSION` | Full |
| `PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION` | `PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION` | Full |
| `REUSABLE_SOROBAN_MODULE_CACHE_PROTOCOL_VERSION` | `REUSABLE_SOROBAN_MODULE_CACHE_PROTOCOL_VERSION` | Full |
| `AUTO_RESTORE_PROTOCOL_VERSION` | `AUTO_RESTORE_PROTOCOL_VERSION` | Full |

### types (`types.rs`)

Corresponds to: `types.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Hash` | `Hash256` | Full |
| `isZero(uint256 const&)` | `Hash256::is_zero()` | Full |
| `operator^=(Hash&, Hash const&)` | `BitXorAssign for Hash256` | Full |
| `LedgerEntryKey()` | `entry_to_key()` | Full |

### asset (`asset.rs`)

Corresponds to: `types.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `isStringValid()` | `is_string_valid()` | Full |
| `isAssetValid<T>()` | `is_asset_valid()`, `is_trustline_asset_valid()`, `is_change_trust_asset_valid()` | Full |
| `getIssuer<T>()` | `get_issuer()`, `get_trustline_asset_issuer()` | Partial |
| `isIssuer<T>()` | `is_issuer()`, `is_trustline_asset_issuer()` | Partial |
| `assetCodeToStr<N>()` | `asset_code_to_str()` | Full |
| `strToAssetCode<N>()` | `str_to_asset_code()` | Full |
| `assetToString()` | `asset_to_string()` | Full |
| `getBucketLedgerKey(HotArchiveBucketEntry)` | `get_hot_archive_bucket_ledger_key()` | Full |
| `getBucketLedgerKey(BucketEntry)` | `get_bucket_ledger_key()` | Full |
| `addBalance()` | `add_balance()` | Full |
| `isAsciiNonControl()` | `is_ascii_non_control()` | Full |

### math (`math.rs`)

Corresponds to: `numeric.h`, `numeric128.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Rounding` enum | `Rounding` enum | Full |
| `isRepresentableAsInt64()` | `is_representable_as_i64()` | Full |
| `doubleToClampedUint32()` | `double_to_clamped_u32()` | Full |
| `bigDivide()` | `big_divide()` | Full |
| `bigDivideUnsigned()` | `big_divide_unsigned()` | Full |
| `bigSquareRoot()` | `big_square_root()` | Full |
| `saturatingMultiply()` | `saturating_multiply()` | Full |
| `bigDivide128()` | `big_divide_128()` | Full |
| `bigDivideUnsigned128()` | `big_divide_unsigned_128()` | Full |
| `bigMultiplyUnsigned()` | `big_multiply_unsigned()` | Full |
| `bigMultiply()` | `big_multiply()` | Full |

### resource (`resource.rs`)

Corresponds to: `TxResource.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Resource` class | `Resource` struct | Full |
| `Resource::Type` enum | `ResourceType` enum | Full |
| `NUM_CLASSIC_TX_RESOURCES` | `NUM_CLASSIC_TX_RESOURCES` | Full |
| `NUM_CLASSIC_TX_BYTES_RESOURCES` | `NUM_CLASSIC_TX_BYTES_RESOURCES` | Full |
| `NUM_SOROBAN_TX_RESOURCES` | `NUM_SOROBAN_TX_RESOURCES` | Full |
| `Resource(vector<int64_t>)` | `Resource::new()` | Full |
| `Resource(int64_t)` | `Resource::new(vec![arg])` | Full |
| `isZero()` | `is_zero()` | Full |
| `anyPositive()` | `any_positive()` | Full |
| `size()` | `size()` | Full |
| `toString()` | `Display for Resource` | Full |
| `getStringFromType()` | `ResourceType::as_str()` | Full |
| `operator+=` | `AddAssign` | Full |
| `operator-=` | `SubAssign` | Full |
| `makeEmptySoroban()` | `make_empty_soroban()` | Full |
| `makeEmpty()` | `make_empty()` | Full |
| `getVal()` | `get_val()` | Full |
| `setVal()` | `set_val()` | Full |
| `canAdd()` | `can_add()` | Full |
| `multiplyByDouble()` | `multiply_by_double()` | Full |
| `saturatedMultiplyByDouble()` | `saturated_multiply_by_double()` | Full |
| `bigDivideOrThrow(Resource)` | `big_divide_resource()` | Full |
| `operator+` | `Add` | Full |
| `operator-` | `Sub` | Full |
| `anyLessThan()` | — | None |
| `anyGreater()` | `any_greater()` | Full |
| `subtractNonNegative()` | `subtract_non_negative()` | Full |
| `limitTo()` | — | None |
| `operator<=` | `leq()` / `PartialOrd` | Full |
| `operator==` | `PartialEq` | Full |
| `operator>` | `PartialOrd` | Full |

### meta (`meta.rs`)

Corresponds to: `MetaUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `normalizeMeta(TransactionMeta&)` | `normalize_transaction_meta()` | Full |
| `normalizeMeta(LedgerCloseMeta&)` | — | None |

### xdr_stream (`xdr_stream.rs`)

Corresponds to: `XDRStream.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `XDROutputFileStream::writeOne()` | `XdrOutputStream::write_one()` | Full |
| `OutputFileStream::open()` | `XdrOutputStream::open()` | Full |
| `OutputFileStream::fdopen()` | `XdrOutputStream::from_fd()` | Full |
| `OutputFileStream::flush()` | `XdrOutputStream::flush()` | Full |
| `XDROutputFileStream::durableWriteOne()` | `DurableXdrOutputStream::durable_write_one()` | Full |
| `XDRInputFileStream::open()` | `XdrInputStream::open()` | Full |
| `XDRInputFileStream::readOne()` | `XdrInputStream::read_one()` | Full |
| `XDRInputFileStream::getXDRSize()` | `decode_frame_size()` / `read_one()` | Full |
| `XDRInputFileStream::readPage()` | — | None |

### fs_utils (`fs_utils.rs`)

Corresponds to: `Fs.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `durableRename()` | `durable_rename()` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `lessThanXored()` (`types.h`) | Implemented where it is used in `henyey-ledger` |
| `compareAsset()`, `unsignedToSigned()`, `formatSize()`, `iequals()`, `isAsciiAlphaNumeric()`, `toAsciiLower()`, `roundDown()` (`types.h`) | Replaced by stdlib helpers or local call-site code |
| `operator>=(Price)`, `operator>(Price)`, `operator==(Price)` (`types.h`) | Implemented in `henyey-tx` where price ordering is needed |
| `bigDivideOrThrow()` (`numeric.h`) | Rust returns `Result` instead of throwing |
| `saturatingAdd<T>()` (`numeric.h`) | Rust integer types provide built-in saturation |
| `bigDivideOrThrow128()` (`numeric128.h`) | Rust returns `Result` instead of throwing |
| `hugeDivide()` (`numeric128.h`) | Logic is inlined in `henyey-tx` pool exchange code |
| `VirtualClock`, `VirtualTimer`, `Scheduler`, `SimpleTimer` (`Timer.h`, `Scheduler.h`, `SimpleTimer.h`) | Runtime timing uses tokio and `henyey-clock` instead |
| `Logging`, `StatusManager`, `MetricsRegistry`, `MetricResetter`, `LogSlowExecution` | Rust uses `tracing` and crate-local metrics infrastructure |
| `Fs` namespace helpers other than `durableRename()` | `std::fs` and per-crate path helpers cover the remaining calls |
| `TmpDir`, `Decoder`, `XDRCereal`, `XDROperators`, `BufferedAsioCerealOutputArchive` | Replaced by Rust crates and derive support |
| `UnorderedMap`, `UnorderedSet`, `RandHasher`, `HashOfHash`, `NonCopyable`, `must_use`, `Thread*`, `asio.h`, `SpdlogTweaks.h`, `Backtrace` | Language/runtime features already provide these roles |
| `RandomEvictionCache`, `BitSet`, `TarjanSCCCalculator`, `BinaryFuseFilter` | Deferred until dependent crates need concrete ports |
| `Math.h` random, clustering, and backoff helpers | Use `rand`/crate-local helpers; not protocol-critical here |
| `xdrquery/*` headers and `DebugMetaUtils.h` | Debug and inspection tooling, not required by the shared common crate |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `ProtocolVersion::V_26` | Low | Enum stops at `V25` even though upstream header now includes `V_26` |
| `protocolVersionEquals()` | Low | Callers currently use direct comparisons |
| `getIssuer<T>()` | Medium | Rust only exposes Asset and TrustLineAsset helpers |
| `isIssuer<T>()` | Medium | Same generic coverage gap as `getIssuer<T>()` |
| `anyLessThan()` (`TxResource.h`) | Medium | Used by resource-limit comparisons in upstream pricing code |
| `limitTo()` (`TxResource.h`) | Medium | Missing clamping helper for per-dimension caps |
| `normalizeMeta(LedgerCloseMeta&)` | Medium | Needed for full ledger-close meta canonicalization |
| `XDRInputFileStream::readPage()` | Medium | Needed for page-based bucket scans and keyed lookup |

## Architectural Differences

1. **Error handling**
   - **stellar-core**: Throws exceptions and uses `releaseAssert` helpers.
   - **Rust**: Returns `Result` for recoverable failures and uses panics for invariants.
   - **Rationale**: Rust surfaces failure paths in types instead of exception control flow.

2. **Stream I/O surface**
   - **stellar-core**: Exposes one hierarchy with explicit open/close, file handles, and seek helpers.
   - **Rust**: Uses narrower wrappers around `BufReader` and `BufWriter` plus RAII cleanup.
   - **Rationale**: The crate implements only the stream operations current Rust callers need.

3. **Resource arithmetic**
   - **stellar-core**: Uses friend functions and operators on a small utility class.
   - **Rust**: Uses trait impls (`Add`, `Sub`, `PartialOrd`) and free helper functions.
   - **Rationale**: Trait-based arithmetic is the idiomatic Rust equivalent.

4. **Filesystem utilities**
   - **stellar-core**: Centralizes many helpers in `fs`.
   - **Rust**: Keeps only crash-sensitive rename logic in `fs_utils.rs` and relies on `std::fs` otherwise.
   - **Rationale**: Most non-durable filesystem helpers do not need a dedicated wrapper in Rust.

5. **Shared support modules**
   - **stellar-core**: Many utility concerns live under `src/util/`.
   - **Rust**: `config.rs`, `network.rs`, `time.rs`, `memory.rs`, and `error.rs` are Rust-native additions without direct upstream peers.
   - **Rationale**: The workspace factors cross-cutting concerns into this crate even when upstream keeps them elsewhere or relies on C++ infrastructure.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Balance helpers | 1 TEST_CASE | 1 `#[test]` | `add_balance()` behavior is covered |
| Big divide and roots | 4 TEST_CASE / 9 SECTION | 15 `#[test]` | Good coverage for 64-bit and 128-bit math |
| Uint128 helpers | 3 TEST_CASE / 6 SECTION | 15 `#[test]` | Rust covers the ported numeric128 helpers, not upstream `uint128_t` internals |
| XDR stream I/O | 2 TEST_CASE / 3 SECTION | 13 `#[test]` | Roundtrip and durable-write coverage is stronger than upstream mapped tests |
| Timer utilities | 8 TEST_CASE (+1 conditional) | 2 `#[test]` | Rust only tests epoch conversion helpers |
| Filesystem rename | 4 TEST_CASE | 3 `#[test]` | Durable rename happy-path and error cases covered |
| Math clustering/random helpers | 1 TEST_CASE / 5 SECTION | 0 `#[test]` | Intentionally omitted from this crate |

### Test Gaps

- `meta.rs` has no direct unit tests for transaction-meta normalization ordering.
- `readPage()` has no Rust coverage because the feature is missing.
- Timer coverage is much thinner because `VirtualClock` and `VirtualTimer` are intentionally not ported here.
- The `getIssuer<T>()` / `isIssuer<T>()` generic gap is not covered by dedicated regression tests.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 69 |
| Gaps (None + Partial) | 8 |
| Intentional Omissions | 15 |
| **Parity** | **69 / (69 + 8) = 90%** |
