# stellar-core Parity Status

**Crate**: `henyey-crypto`
**Upstream**: `stellar-core/src/crypto/`
**Overall Parity**: 59%
**Last Updated**: 2026-04-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| SHA-256 Hashing | Partial | Single-shot, streaming (no reset), HMAC, HKDF, XDR hashing |
| BLAKE2 Hashing | Partial | Single-shot only; no streaming hasher or XDR hashing |
| Hex Encoding | None | Intentionally omitted; call sites use the `hex` crate directly |
| Random Generation | Full | CSPRNG via OsRng |
| Curve25519 ECDH | Full | Key exchange, shared key derivation |
| Sealed Box Encryption | Full | Encrypt/decrypt via crypto_box |
| Ed25519 Keys & Signatures | Partial | Generate, sign, verify, StrKey; missing isZero, ==, < |
| StrKey Encoding | Partial | Core encode/decode via `stellar_strkey`; missing size/convert utils |
| Short Hash (SipHash) | Full | initialize, computeHash, xdrComputeHash, seed |
| SignerKey Utilities | None | No crate-local constructors; transaction code builds XDR directly |
| Signature Verification Cache | Partial | BLAKE2-keyed FIFO cache; missing clear/seed/flush APIs |
| Key/Logging Utilities | None | toShortString, logKey, random helpers |
| Error Types | Full | `CryptoError` enum covers all upstream failure modes |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `SHA.h` / `SHA.cpp` | `hash.rs` | SHA-256, HMAC-SHA256, HKDF |
| `BLAKE2.h` / `BLAKE2.cpp` | `hash.rs` | BLAKE2b-256 single-shot only |
| `Hex.h` / `Hex.cpp` | *(omitted)* | Direct `hex` crate use at call sites |
| `Random.h` / `Random.cpp` | `random.rs` | CSPRNG |
| `Curve25519.h` / `Curve25519.cpp` | `curve25519.rs`, `sealed_box.rs` | ECDH and sealed box split across two modules |
| `SecretKey.h` / `SecretKey.cpp` | `keys.rs`, `signature.rs` | Key types, signing, verification with cache |
| `StrKey.h` / `StrKey.cpp` | *(re-export)* `stellar_strkey` | StrKey encoding/decoding delegated to external crate |
| `ShortHash.h` / `ShortHash.cpp` | `short_hash.rs` | SipHash-2-4 |
| `SignerKey.h` / `SignerKey.cpp` | *(gap)* | No crate-local SignerKey utility module |
| `SignerKeyUtils.h` / `SignerKeyUtils.cpp` | *(gap)* | No crate-local SignerKey construction helpers |
| `KeyUtils.h` / `KeyUtils.cpp` | `keys.rs` | Generic key encode/decode via methods on key types |
| `CryptoError.h` | `error.rs` | Error types |
| `XDRHasher.h` | `hash.rs` | XDR hashing (different approach) |

## Component Mapping

### hash.rs (`hash.rs`)

Corresponds to: `SHA.h`, `BLAKE2.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `sha256(ByteSlice)` | `sha256(&[u8])` | Full |
| `subSha256(ByteSlice, uint64)` | `sub_sha256(&[u8], u64)` | Full |
| `SHA256::SHA256()` | `Sha256Hasher::new()` | Full |
| `SHA256::reset()` | -- | None |
| `SHA256::add(ByteSlice)` | `Sha256Hasher::update(&[u8])` | Full |
| `SHA256::finish()` | `Sha256Hasher::finalize()` | Full |
| `xdrSha256<T>(T)` | `xdr_sha256<T: WriteXdr>(T)` (`#[cfg(test)]`) | Full |
| `hmacSha256(key, bin)` | `hmac_sha256(&[u8; 32], &[u8])` (`pub(crate)`) | Full |
| `hmacSha256Verify(hmac, key, bin)` | `hmac_sha256_verify(&[u8; 32], &[u8; 32], &[u8])` (`#[cfg(test)]`) | Full |
| `hkdfExtract(bin)` | `hkdf_extract(&[u8])` (`pub(crate)`) | Full |
| `hkdfExpand(key, bin)` | `hkdf_expand(&[u8; 32], &[u8])` (`#[cfg(test)]`) | Full |
| `blake2(ByteSlice)` | `blake2(&[u8])` | Full |
| `BLAKE2::BLAKE2()` | -- | None |
| `BLAKE2::reset()` | -- | None |
| `BLAKE2::add(ByteSlice)` | -- | None |
| `BLAKE2::finish()` | -- | None |
| `xdrBlake2<T>(T)` | -- | None |

### Hex utilities (omitted)

Corresponds to: `Hex.h`

No crate-local hex module exists. Rust call sites use the `hex` crate directly; this is listed under Intentional Omissions and excluded from the parity calculation.

| stellar-core | Rust | Status |
|--------------|------|--------|
| `binToHex(ByteSlice)` | `hex::encode(...)` at call sites | None |
| `hexAbbrev(ByteSlice)` | direct formatting at call sites | None |
| `hexToBin(string)` | `hex::decode(...)` at call sites | None |
| `hexToBin256(string)` | `hex::decode(...)` plus length checks at call sites | None |

### random.rs (`random.rs`)

Corresponds to: `Random.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `randomBytes(size_t)` | `random_bytes::<N>()` / `fill_random(&mut [u8])` | Full |

### curve25519.rs (`curve25519.rs`)

Corresponds to: `Curve25519.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `curve25519RandomSecret()` | `Curve25519Secret::random()` | Full |
| `curve25519DerivePublic(secret)` | `Curve25519Secret::derive_public()` | Full |
| `curve25519DeriveSharedKey(sec, lpub, rpub, first)` | `Curve25519Secret::derive_shared_key(sec, lpub, rpub, first)` | Full |
| `hash<Curve25519Public>::operator()` | `Hash for Curve25519Public` | Full |
| `crypto_scalarmult` contributory check (reject small-order) | `Curve25519Secret::diffie_hellman` + `check_public_key_contributory` | Full |

### sealed_box.rs (`sealed_box.rs`)

Corresponds to: `Curve25519.h` (encrypt/decrypt portion)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `curve25519Encrypt<N>(pub, bin)` | `seal_to_public_key(pub, &[u8])` / `seal_to_curve25519_public_key(&[u8;32], &[u8])` | Full |
| `curve25519Decrypt(sec, pub, enc)` | `open_from_secret_key(sec, &[u8])` / `open_from_curve25519_secret_key(&[u8;32], &[u8])` | Full |
| Small-order Curve25519 key rejection | `check_public_key_contributory` + `ContributoryPublicKey` newtype | Full |
| `crypto_scalarmult` contributory check (via `was_contributory`) | `Curve25519Secret::diffie_hellman` | Full |

### keys.rs (`keys.rs`)

Corresponds to: `SecretKey.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SecretKey::SecretKey()` | `SecretKey::from_seed(&[u8; 32])` | Full |
| `SecretKey::~SecretKey()` | Drop (zeroize via ed25519-dalek) | Full |
| `SecretKey::getPublicKey()` | `SecretKey::public_key()` | Full |
| `SecretKey::getStrKeySeed()` | `SecretKey::to_strkey()` | Full |
| `SecretKey::getStrKeyPublic()` | `PublicKey::to_strkey()` | Full |
| `SecretKey::isZero()` | -- | None |
| `SecretKey::sign(ByteSlice)` | `SecretKey::sign(&[u8])` | Full |
| `SecretKey::random()` | `SecretKey::generate()` | Full |
| `SecretKey::fromStrKeySeed(string)` | `SecretKey::from_strkey(&str)` | Full |
| `SecretKey::fromSeed(ByteSlice)` | `SecretKey::from_seed(&[u8; 32])` | Full |
| `SecretKey::operator==` | -- | None |
| `SecretKey::operator<` | -- | None |
| `hash<PublicKey>::operator()` | `Hash for PublicKey` (derived) | Full |

### signature.rs (`signature.rs`)

Corresponds to: `SecretKey.h` (PubKeyUtils verification)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PubKeyUtils::verifySig(key, sig, bin)` | `verify_hash_from_raw_key(&[u8;32], &Hash256, &Signature)` | Full |
| `PubKeyUtils::clearVerifySigCache()` | -- | None |
| `PubKeyUtils::seedVerifySigCache(seed)` | -- | None |
| `PubKeyUtils::flushVerifySigCacheCounts(hits, misses)` | -- | None |
| `PubKeyUtils::random()` | -- | None |
| `StrKeyUtils::logKey(ostream, key)` | -- | None |
| `HashUtils::random()` | -- | None |

### StrKey (via `stellar_strkey` re-export)

Corresponds to: `StrKey.h`, `KeyUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `strKey::toStrKey(ver, bin)` | `stellar_strkey` crate (internal) | Full |
| `strKey::fromStrKey(s, ver, decoded)` | `stellar_strkey` crate (internal) | Full |
| `strKey::getStrKeySize(dataSize)` | -- | None |
| `KeyUtils::toStrKey<T>(key)` | `PublicKey::to_strkey()`, `SecretKey::to_strkey()` | Full |
| `KeyUtils::fromStrKey<T>(s)` | `PublicKey::from_strkey()`, `SecretKey::from_strkey()` | Full |
| `KeyUtils::toShortString<T>(key)` | -- | None |
| `KeyUtils::getKeyVersionSize(ver)` | -- | None |
| `KeyUtils::canConvert<T,F>(key)` | -- | None |
| `KeyUtils::convertKey<T,F>(key)` | -- | None |

### short_hash.rs (`short_hash.rs`)

Corresponds to: `ShortHash.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `shortHash::initialize()` | `initialize()` (`#[cfg(test)]`) | Full |
| `shortHash::getShortHashInitKey()` | -- | None |
| `shortHash::seed(unsigned int)` | `seed(u32)` | Full |
| `shortHash::computeHash(ByteSlice)` | `compute_hash(&[u8])` (`pub(crate)`) | Full |
| `shortHash::xdrComputeHash<T>(T)` | `xdr_compute_hash<T: WriteXdr>(T)` | Full |

### SignerKey utilities (not implemented)

Corresponds to: `SignerKeyUtils.h`, `SignerKey.h`

No crate-local SignerKey utility module exists. Transaction validation code constructs and matches XDR `SignerKey` values directly where needed.

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SignerKeyUtils::preAuthTxKey(TransactionFrame)` | -- | None |
| `SignerKeyUtils::preAuthTxKey(FeeBumpTransactionFrame)` | -- | None |
| `SignerKeyUtils::hashXKey(ByteSlice)` | -- | None |
| `SignerKeyUtils::ed25519PayloadKey(uint256, payload)` | -- | None |
| `KeyFunctions<SignerKey>::getEd25519Value()` | -- | None |

### error.rs (`error.rs`)

Corresponds to: `CryptoError.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CryptoError` exception class | `CryptoError` enum | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `ByteSlice` adaptor class | Rust native `&[u8]` slices serve the same purpose |
| `XDRHasher<T>` CRTP base class | Rust uses allocation-based XDR serialization; the public `xdrSha256` function is implemented differently |
| `XDRSHA256::hashBytes()` | Internal CRTP implementation detail; public API `xdrSha256<T>` is fully implemented |
| `XDRBLAKE2::hashBytes()` | Internal CRTP implementation detail |
| `XDRShortHasher` (zero-alloc streaming) | Performance optimization; `xdr_compute_hash()` provides equivalent functionality via allocation |
| `clearCurve25519Keys(pub, sec)` | Rust uses `ZeroizeOnDrop` trait; keys are automatically zeroed when dropped |
| `KeyFunctions<PublicKey>` template specialization | Functionality covered by direct `PublicKey` method implementations |
| `KeyFunctions<SignerKey>` (5 of 7 methods) | `getKeyTypeName`, `getKeyVersionIsSupported`, `toKeyType`, `toKeyVersion`, `getKeyValue`, `setKeyValue` are replaced by Rust match expressions and direct construction |
| `SecretKey::benchmarkOpsPerSecond()` | Benchmark-only utility, not needed for correctness |
| `SecretKey::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard), Rust tests use `SecretKey::generate()` |
| `SecretKey::pseudoRandomForTestingFromSeed()` | Test-only (`BUILD_TESTS` guard) |
| `PubKeyUtils::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard) |
| `HashUtils::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard) |
| `PubKeyUtils::enableRustDalekVerify()` | Protocol migration helper; Rust implementation always uses ed25519-dalek |
| `binToHex()`, `hexAbbrev()`, `hexToBin()`, `hexToBin256()` | Utility formatting/parsing is handled by the Rust `hex` crate directly at call sites |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `SHA256::reset()` | Low | Streaming hasher consumes on finalize; create new instance instead |
| `BLAKE2::BLAKE2()` (streaming) | Medium | No streaming BLAKE2 hasher exists; only single-shot `blake2()` |
| `BLAKE2::reset()` | Medium | Depends on streaming BLAKE2 hasher |
| `BLAKE2::add()` | Medium | Depends on streaming BLAKE2 hasher |
| `BLAKE2::finish()` | Medium | Depends on streaming BLAKE2 hasher |
| `xdrBlake2<T>()` | Medium | XDR BLAKE2 hashing not implemented |
| `SecretKey::isZero()` | Low | Simple utility; can use `as_bytes() == &[0u8; 32]` |
| `SecretKey::operator==` | Low | Compare via `as_bytes()` but no `PartialEq` impl |
| `SecretKey::operator<` | Low | Ordering comparison for sorting |
| `PubKeyUtils::clearVerifySigCache()` | Low | Cache exists but no public clear API |
| `PubKeyUtils::seedVerifySigCache()` | Low | Cache exists but no public seed API |
| `PubKeyUtils::flushVerifySigCacheCounts()` | Low | Cache exists but no hit/miss counters |
| `PubKeyUtils::random()` | Low | Random public key generation utility |
| `HashUtils::random()` | Low | Random hash generation utility |
| `StrKeyUtils::logKey()` | Low | Logging utility for key inspection |
| `strKey::getStrKeySize()` | Low | Computes encoded StrKey size from data size |
| `SignerKeyUtils::preAuthTxKey(TransactionFrame)` | Medium | No crate-local SignerKey helper module |
| `SignerKeyUtils::preAuthTxKey(FeeBumpTransactionFrame)` | Medium | No crate-local SignerKey helper module |
| `SignerKeyUtils::hashXKey(ByteSlice)` | Medium | No crate-local SignerKey helper module |
| `SignerKeyUtils::ed25519PayloadKey(uint256, payload)` | Medium | No crate-local SignerKey helper module |
| `KeyFunctions<SignerKey>::getEd25519Value()` | Medium | No crate-local SignerKey helper module |
| `shortHash::getShortHashInitKey()` | Low | Returns current SipHash key (diagnostic) |
| `KeyUtils::toShortString()` | Low | Short string representation for logging |
| `KeyUtils::getKeyVersionSize()` | Low | Returns expected payload size for a version byte |
| `KeyUtils::canConvert()` | Low | Check if key type conversion is possible |
| `KeyUtils::convertKey()` | Low | Convert between compatible key types |

## Architectural Differences

1. **No libsodium dependency**
   - **stellar-core**: Uses libsodium (C library) for all cryptographic operations
   - **Rust**: Uses pure Rust crates (`ed25519-dalek`, `sha2`, `blake2`, `hmac`, `siphasher`, `crypto_box`, `x25519-dalek`)
   - **Rationale**: Eliminates C FFI, enables reproducible builds, and provides Rust memory safety guarantees

2. **XDR hashing approach**
   - **stellar-core**: Uses `XDRHasher<T>` CRTP pattern that streams XDR serialization directly into the hash function with a 256-byte buffer, achieving zero allocation
   - **Rust**: Serializes XDR to `Vec<u8>` via `stellar_xdr` crate, then hashes the buffer
   - **Rationale**: Simpler implementation at the cost of one allocation per hash; acceptable for most use cases

3. **Signature verification cache**
   - **stellar-core**: Maintains a process-wide `RandomEvictionCache<Hash, bool>` that caches verification results keyed by BLAKE2 hash of (key, sig, data), with hit/miss counters and seed/clear/flush management APIs
   - **Rust**: Maintains a process-wide `SigVerifyCache` with a 250K-entry FIFO eviction policy, keyed by BLAKE2 hash of (pubkey, sig, hash). Missing: clear API, seed API, and hit/miss counter flushing
   - **Rationale**: Core caching mechanism is implemented for performance; management APIs not yet needed

4. **Key type template system**
   - **stellar-core**: Uses `KeyFunctions<T>` template specializations to provide generic StrKey encode/decode for `PublicKey`, `SecretKey`, and `SignerKey`
   - **Rust**: Provides separate typed functions and dedicated methods on key types; StrKey encoding delegated to `stellar_strkey` crate
   - **Rationale**: Rust's type system and trait implementations replace the need for C++ template specializations; the specific function names are more discoverable

5. **Error handling**
   - **stellar-core**: Throws C++ exceptions (`CryptoError`, `std::runtime_error`, `InvalidStrKey`)
   - **Rust**: Returns `Result<T, CryptoError>` with typed error variants
   - **Rationale**: Idiomatic Rust error handling; all failure modes are explicit in function signatures

6. **Streaming hasher lifecycle**
   - **stellar-core**: `SHA256` and `BLAKE2` have `reset()` methods allowing reuse of the hasher object
   - **Rust**: `Sha256Hasher::finalize()` consumes the hasher; create a new instance to hash again
   - **Rationale**: Rust's ownership model makes consuming-finalize more natural; no `BLAKE2` streaming hasher exists yet

7. **Crate-private and `#[cfg(test)]` helpers**
   - **stellar-core**: All crypto functions are available in both production and test builds
   - **Rust**: Several individual helpers (`hmac_sha256_verify`, `hkdf_expand`, `xdr_sha256`, Ed25519 sealed-box wrappers, `initialize`, random test helpers) are crate-private or compiled only in tests. Production builds use higher-level wrappers or the `pub(crate)` internal API directly
   - **Rationale**: Minimizes public API surface; functions only used internally or in tests are not exposed to downstream crates

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| SHA-256 Hashing | 5 TEST_CASE (incl. 2 bench) | 15 #[test] | Rust has more granular tests |
| BLAKE2 Hashing | 5 TEST_CASE (incl. 2 bench) | 3 #[test] | BLAKE2-specific tests in hash.rs |
| HMAC / HKDF | 2 TEST_CASE | 7 #[test] | Good coverage on both sides |
| Signing / Verification | 5 TEST_CASE (incl. 2 bench, 2 vector suites) | 5 #[test] (keys + signature) | Upstream has much deeper vector coverage |
| StrKey | 2 TEST_CASE, 5 SECTION | Covered by `stellar_strkey` | Rust relies on external crate's tests |
| Short Hash | 3 TEST_CASE (incl. 2 bench) | 5 #[test] | Comparable coverage |
| Hex | Included in CryptoTests | 0 #[test] | Intentionally omitted; direct `hex` crate use |
| Curve25519 / ECDH | -- | 8 #[test] | Upstream tests are in overlay |
| Random | 1 TEST_CASE | 3 #[test] | Basic non-determinism checks |
| SignerKey | 1 TEST_CASE, 5 SECTION | 0 #[test] | Helper module not implemented |
| Sealed Box | -- | 8 #[test] | Upstream tests in overlay |
| Sig Verification Cache | Included in verify tests | 2 #[test] | Basic cache hit/miss tests |

### Test Gaps

- **Ed25519 test vectors**: Upstream has extensive test vectors from IACR 2020/1244 (1 TEST_CASE, many sub-vectors) and Zcash (1 TEST_CASE). The Rust crate relies on `ed25519-dalek`'s own test suite for these vectors rather than replicating them.
- **StrKey edge cases**: Upstream `CryptoTests.cpp` has a large `TEST_CASE("StrKey tests")` block with many edge cases for invalid StrKeys. The Rust tests rely on `stellar_strkey`'s test suite.
- **Benchmark tests**: Upstream has 6 benchmark `TEST_CASE` entries. The Rust crate has no benchmark tests.
- **Cache management**: No tests for cache clearing, seeding, or hit/miss counting (APIs not yet implemented).
- **SignerKey helpers**: No crate-local helpers or tests for upstream `SignerKeyUtils`; transaction validation matches XDR values directly.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 38 |
| Gaps (None + Partial) | 26 |
| Intentional Omissions | 18 |
| **Parity** | **38 / (38 + 26) = 59%** |
