# stellar-core Parity Status

**Crate**: `henyey-common`
**Upstream**: `stellar-core/src/util/`
**Overall Parity**: 99%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Protocol Version | Full | All constants and comparisons match |
| Types / Hash | Full | Hash256, XOR, zero-check |
| Asset Utilities | Full | Validation, issuer, balance, price ops |
| Numeric (64-bit) | Full | bigDivide, saturating ops, sqrt |
| Numeric (128-bit) | Full | bigDivide128, bigMultiply |
| Resource Accounting | Full | All Resource methods and friends |
| Metadata Normalization | Full | Sorting matches stellar-core exactly |
| XDR Output Stream | Full | writeOne with size-prefix framing |
| XDR Durable Output | Full | durableWriteOne with fsync |
| XDR Input Stream | Partial | readOne implemented; readPage missing |
| Filesystem Utilities | Full | durableRename with directory fsync |
| Network Identity | Full | Passphrase-based derivation |
| Time Utilities | Full | Epoch conversions implemented |
| Configuration | Full | Rust-native TOML approach |
| Error Handling | Full | Rust-native Result/Error approach |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `ProtocolVersion.h` / `ProtocolVersion.cpp` | `protocol.rs` | Full parity |
| `types.h` / `types.cpp` | `asset.rs`, `types.rs` | Full parity for protocol functions; utility functions covered by Rust stdlib |
| `numeric.h` / `numeric.cpp` | `math.rs` | Full parity |
| `numeric128.h` | `math.rs` | `hugeDivide` inlined in henyey-tx |
| `TxResource.h` / `TxResource.cpp` | `resource.rs` | Full parity |
| `MetaUtils.h` / `MetaUtils.cpp` | `meta.rs` | Full parity |
| `XDRStream.h` | `xdr_stream.rs` | Output + durable full; input partial (readPage missing) |
| `Fs.h` / `Fs.cpp` | `fs_utils.rs` | `durableRename` only; other Fs functions intentionally omitted |
| `Math.h` / `Math.cpp` | — | Random/clustering utilities intentionally omitted |

## Component Mapping

### protocol (`protocol.rs`)

Corresponds to: `ProtocolVersion.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ProtocolVersion` enum (V_0..V_25) | `ProtocolVersion` enum (V0..V25) | Full |
| `protocolVersionIsBefore()` | `protocol_version_is_before()` | Full |
| `protocolVersionStartsFrom()` | `protocol_version_starts_from()` | Full |
| `protocolVersionEquals()` | `protocol_version_equals()` | Full |
| `SOROBAN_PROTOCOL_VERSION` | `SOROBAN_PROTOCOL_VERSION` | Full |
| `PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION` | `PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION` | Full |
| `REUSABLE_SOROBAN_MODULE_CACHE_PROTOCOL_VERSION` | `REUSABLE_SOROBAN_MODULE_CACHE_PROTOCOL_VERSION` | Full |
| `AUTO_RESTORE_PROTOCOL_VERSION` | `AUTO_RESTORE_PROTOCOL_VERSION` | Full |

### types (`types.rs`)

Corresponds to: `types.h` (Hash / uint256 portion)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Hash` (xdr::opaque_array<32>) | `Hash256` | Full |
| `isZero(uint256 const&)` | `Hash256::is_zero()` | Full |
| `operator^=` for Hash | `BitXorAssign` for Hash256 | Full |
| `lessThanXored()` | `less_than_xored()` | Full |

### asset (`asset.rs`)

Corresponds to: `types.h` (asset / balance / utility portion)

#### Asset Validation and Conversion

| stellar-core | Rust | Status |
|--------------|------|--------|
| `isAssetValid<T>()` | `is_asset_valid()` | Full |
| `isTrustLineAssetValid()` | `is_trustline_asset_valid()` | Full |
| `isChangeTrustAssetValid()` | `is_change_trust_asset_valid()` | Full |
| `getIssuer<T>()` | `get_issuer()` | Full |
| `isIssuer<T>()` | `is_issuer()` | Full |
| `assetCodeToStr<N>()` | `asset_code_to_str()` | Full |
| `strToAssetCode<N>()` | `str_to_asset_code()` | Full |
| `assetToString()` | `asset_to_string()` | Full |
| `isStringValid()` | `is_string_valid()` | Full |
| `isAsciiNonControl()` | `is_ascii_non_control()` | Full |

#### Type Utilities

| stellar-core | Rust | Status |
|--------------|------|--------|
| `LedgerEntryKey()` | `ledger_entry_key()` | Full |
| `getBucketLedgerKey(BucketEntry)` | `get_bucket_ledger_key()` | Full |
| `getBucketLedgerKey(HotArchiveBucketEntry)` | `get_hot_archive_bucket_ledger_key()` | Full |
| `addBalance()` | `add_balance()` | Full |
| `operator>=(Price)` | `price_ge()` | Full |
| `operator>(Price)` | `price_gt()` | Full |

### math (`math.rs`)

Corresponds to: `numeric.h`, `numeric128.h`

#### 64-bit Arithmetic (numeric.h)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Rounding` enum | `Rounding` enum | Full |
| `isRepresentableAsInt64()` | `is_representable_as_i64()` | Full |
| `doubleToClampedUint32()` | `double_to_clamped_u32()` | Full |
| `bigDivide()` | `big_divide()` | Full |
| `bigDivideUnsigned()` | `big_divide_unsigned()` | Full |
| `bigSquareRoot()` | `big_square_root()` | Full |
| `saturatingMultiply()` | `saturating_multiply()` | Full |

#### 128-bit Arithmetic (numeric128.h)

| stellar-core | Rust | Status |
|--------------|------|--------|
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
| `getVal()` | `get_val()` / `try_get_val()` | Full |
| `setVal()` | `set_val()` / `try_set_val()` | Full |
| `makeEmpty()` | `make_empty()` | Full |
| `makeEmptySoroban()` | `make_empty_soroban()` | Full |
| `canAdd()` | `can_add()` | Full |
| `toString()` | `Display` impl | Full |
| `getStringFromType()` | `ResourceType::as_str()` | Full |
| `operator+=` | `AddAssign` impl | Full |
| `operator-=` | `SubAssign` impl | Full |
| `operator+` | `Add` impl | Full |
| `operator-` | `Sub` impl | Full |
| `operator<=` | `leq()` / `PartialOrd` | Full |
| `operator==` | `PartialEq` impl | Full |
| `operator>` | `PartialOrd` impl | Full |
| `anyLessThan()` | `any_less_than()` | Full |
| `anyGreater()` | `any_greater()` | Full |
| `subtractNonNegative()` | `subtract_non_negative()` | Full |
| `limitTo()` | `limit_to()` | Full |
| `multiplyByDouble()` | `multiply_by_double()` | Full |
| `saturatedMultiplyByDouble()` | `saturated_multiply_by_double()` | Full |
| `bigDivideOrThrow()` (Resource) | `big_divide_resource()` | Full |

### meta (`meta.rs`)

Corresponds to: `MetaUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `normalizeMeta(TransactionMeta&)` | `normalize_transaction_meta()` | Full |
| `normalizeMeta(LedgerCloseMeta&)` | `normalize_ledger_close_meta()` | Full |

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
| `XDRInputFileStream::getXDRSize()` | Inlined in `read_one()` | Full |
| `XDRInputFileStream::readPage()` | — | None |

### fs_utils (`fs_utils.rs`)

Corresponds to: `Fs.h` (partial)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `durableRename()` | `durable_rename()` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `compareAsset()` (`types.h`) | Rust XDR types derive `PartialEq`; direct `==` comparison works |
| `unsignedToSigned(uint32_t)` / `unsignedToSigned(uint64_t)` (`types.h`) | Rust `TryFrom` / `as` casts; no wrapper needed |
| `formatSize()` (`types.h`) | Formatting utility; not protocol-critical |
| `iequals()` (`types.h`) | Rust `str::eq_ignore_ascii_case()` in stdlib |
| `roundDown<T>()` (`types.h`) | Implemented locally where needed (bucket crate) |
| `isAsciiAlphaNumeric()` (`types.h`) | Rust `char::is_ascii_alphanumeric()` in stdlib |
| `toAsciiLower()` (`types.h`) | Rust `char::to_ascii_lowercase()` in stdlib |
| `operator==(Price)` (`types.h`) | Rust XDR types derive `PartialEq` |
| `bigDivideOrThrow()` (`numeric.h`) | Rust `big_divide()` returns `Result`; caller uses `?` or `.unwrap()` |
| `saturatingAdd<T>()` (`numeric.h`) | Rust `u64::saturating_add()` in stdlib |
| `bigDivideOrThrow128()` (`numeric128.h`) | `big_divide_128()` returns `Result`; same pattern |
| `hugeDivide()` (`numeric128.h`) | Inlined in `exchange_with_pool()` in `henyey-tx` |
| `VirtualClock` / `VirtualTimer` (`Timer.h`) | Rust uses tokio async runtime instead |
| `Scheduler` (`Scheduler.h`) | Handled by tokio task scheduling |
| `Logging` / `CoutLogger` / CLOG macros (`Logging.h`) | Deferred to Rust `tracing` crate ecosystem |
| `Fs` namespace (`Fs.h`) (except `durableRename`) | Rust `std::fs` and `std::path` suffice |
| `GlobalChecks` / `releaseAssert` (`GlobalChecks.h`) | Rust `assert!` / `panic!` are built-in |
| `NonCopyable` / `NonMovable` (`NonCopyable.h`) | Rust ownership system handles this natively |
| `BacktraceManager` (`Backtrace.h`) | Rust has `std::backtrace::Backtrace` |
| `Thread` utilities (`Thread.h`) | Rust has `std::thread` and async runtimes |
| `SecretValue` (`SecretValue.h`) | Trivial wrapper; Rust uses `secrecy` crate when needed |
| `StatusManager` (`StatusManager.h`) | Different monitoring architecture in Rust |
| `TmpDir` / `TmpDirManager` (`TmpDir.h`) | Rust has `tempfile` crate |
| `Decoder` base32/base64 (`Decoder.h`) | Rust has `base64` / `base32` crates |
| `XDRCereal` JSON serialization (`XDRCereal.h`) | Rust uses `serde_json` with XDR types |
| `XDROperators` (`XDROperators.h`) | Rust derives `PartialEq`, `PartialOrd` via `stellar-xdr` |
| `BufferedAsioCerealOutputArchive` (`BufferedAsioCerealOutputArchive.h`) | C++ serialization library integration |
| `UnorderedMap` / `UnorderedSet` (`UnorderedMap.h`, `UnorderedSet.h`) | Rust `HashMap` / `HashSet` with built-in DoS protection |
| `RandHasher` (`RandHasher.h`) | Rust `HashMap` uses SipHash by default |
| `HashOfHash` (`HashOfHash.h`) | Rust `Hash` trait covers this |
| `Algorithm` split helper (`Algorithm.h`) | Trivial; use `itertools::group_by` or manual grouping |
| `must_use` macro (`must_use.h`) | Rust `#[must_use]` attribute is built-in |
| `ThreadAnnotations` (`ThreadAnnotations.h`) | Rust borrow checker provides thread safety statically |
| `SpdlogTweaks` (`SpdlogTweaks.h`) | C++ logging library configuration |
| `FileSystemException` (`FileSystemException.h`) | Rust uses `std::io::Error` |
| `asio.h` | C++ async IO library wrapper; Rust uses tokio |
| `RandomEvictionCache` (`RandomEvictionCache.h`) | Data structure; implement when needed by dependent crates |
| `BitSet` (`BitSet.h`) | Data structure; implement when needed (e.g., for SCP) |
| `TarjanSCCCalculator` (`TarjanSCCCalculator.h`) | Implement when needed by dependent crates |
| `BinaryFuseFilter` (`BinaryFuseFilter.h`) | Bucket-specific; handle in bucket crate |
| `MetricResetter` (`MetricResetter.h`) | Different metrics infrastructure in Rust |
| `LogSlowExecution` (`LogSlowExecution.h`) | Defer to Rust `tracing` spans with timing |
| `JitterInjection` (`JitterInjection.h`) | Testing-only jitter injection; not needed |
| `DebugMetaUtils` (`DebugMetaUtils.h`) | Debug infrastructure; implement when needed |
| `Math.h` random utilities (`rand_fraction`, `rand_flip`, `rand_uniform`, `rand_element`, etc.) | Rust uses `rand` crate |
| `Math.h` `k_means` / `closest_cluster` | Operational utility; not protocol-critical |
| `Math.h` `exponentialBackoff` | Operational utility; Rust has `backoff` crate |
| `Math.h` `initializeAllGlobalState` | C++ global state init; not applicable in Rust |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `XDRInputFileStream::readPage()` | Low | Page-based reading with key search; BucketListDB optimization |

## Architectural Differences

1. **Error Handling**
   - **stellar-core**: C++ exceptions and `releaseAssert` macros
   - **Rust**: `Result<T, E>` for recoverable errors, `panic!` for invariant violations
   - **Rationale**: Rust's type system enforces error handling at compile time

2. **Configuration Parsing**
   - **stellar-core**: Custom text-based config parser
   - **Rust**: `serde` + `toml` crate with strongly-typed structs
   - **Rationale**: Industry-standard approach; better validation and documentation

3. **Resource Arithmetic**
   - **stellar-core**: Friend functions and operator overloads on a class
   - **Rust**: Trait implementations (`Add`, `Sub`, `PartialOrd`) plus free functions
   - **Rationale**: Idiomatic Rust; trait implementations enable generic code

4. **Hash Type**
   - **stellar-core**: `typedef` over XDR opaque array with free functions
   - **Rust**: `Hash256` newtype wrapper with methods and trait implementations
   - **Rationale**: Newtype pattern provides type safety and method namespace

5. **XDR Stream I/O**
   - **stellar-core**: ASIO-based buffered streams with a single class hierarchy
   - **Rust**: Separate `XdrOutputStream` and `DurableXdrOutputStream` types; `BufWriter`/`BufReader` from stdlib
   - **Rationale**: Rust's type system makes separate types clearer than runtime flags

6. **Durable Filesystem Operations**
   - **stellar-core**: `Fs` namespace with many filesystem utilities
   - **Rust**: Only `durable_rename` implemented; other Fs operations use `std::fs` directly
   - **Rationale**: Rust stdlib covers most filesystem operations; only `durableRename` has non-trivial crash-safety semantics

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Balance | 1 TEST_CASE | 1 #[test] | Covered |
| BigDivide | 4 TEST_CASE / 8 SECTION | 10 #[test] | Good coverage |
| Uint128 | 3 TEST_CASE / 5 SECTION | 2 #[test] | Adequate |
| XDRStream | 2 TEST_CASE / 3 SECTION | 10 #[test] | Output, durable, and input roundtrip coverage |
| Timer | 8 TEST_CASE | 2 #[test] | Rust covers epoch conversions only |
| Filesystem | 4 TEST_CASE | 3 #[test] | `durable_rename` basic, overwrite, error cases |
| Math (random) | 1 TEST_CASE / 5 SECTION | 0 #[test] | Intentionally omitted (rand crate) |
| Cache | 8 TEST_CASE | 0 #[test] | Intentionally omitted |
| BitSet | 6 TEST_CASE | 0 #[test] | Intentionally omitted |
| Decoder | 9 TEST_CASE | 0 #[test] | Intentionally omitted |
| Scheduler | 2 TEST_CASE | 0 #[test] | Intentionally omitted |
| StatusManager | 5 TEST_CASE | 0 #[test] | Intentionally omitted |
| Metrics | 12 TEST_CASE | 0 #[test] | Intentionally omitted |
| BinaryFuse | 1 TEST_CASE / 3 SECTION | 0 #[test] | Intentionally omitted |
| Protocol | — | 4 #[test] | Rust-only tests |
| Types/Hash | — | 3 #[test] | Rust-only tests |
| Asset | — | 19 #[test] | Rust-only tests |
| Resource | — | 13 #[test] | Rust-only tests |
| Network | — | 3 #[test] | Rust-only tests |
| Meta | — | 0 #[test] | No unit tests; relies on integration tests |

### Test Gaps

- **Metadata normalization**: No dedicated unit tests in `meta.rs`; upstream has no dedicated test file either, but parity is verified through integration testing.
- **XDR readPage**: `readPage()` not tested because not implemented.
- **hugeDivide**: Algorithm inlined in `henyey-tx` `exchange_with_pool()`; tested via offer exchange tests.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 76 |
| Gaps (None) | 1 |
| Intentional Omissions | 47 |
| **Parity** | **76 / (76 + 1) = 99%** |
