# stellar-core Parity Status

**Crate**: `henyey-crypto`
**Upstream**: `stellar-core/src/crypto/`
**Overall Parity**: 78%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| SHA-256 Hashing | Full | All functions including streaming, HMAC, HKDF |
| BLAKE2 Hashing | Full | Single-shot, streaming, and XDR hashing |
| Hex Encoding | Full | All four upstream functions implemented |
| Random Generation | Full | CSPRNG via OsRng |
| Curve25519 ECDH | Full | Key exchange, shared key derivation |
| Sealed Box Encryption | Full | Encrypt/decrypt via crypto_box |
| Ed25519 Keys & Signatures | Full | Generate, sign, verify, StrKey encode/decode |
| StrKey Encoding | Full | Via re-exported `stellar_strkey` crate |
| Short Hash (SipHash) | Full | computeHash, xdrComputeHash, seed management |
| SignerKey Utilities | Full | preAuthTx, hashX, ed25519Payload |
| Signature Verification Cache | None | Performance optimization, not correctness |
| Key/Logging Utilities | None | toShortString, logKey, canConvert, etc. |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `SHA.h` / `SHA.cpp` | `hash.rs` | SHA-256, HMAC-SHA256, HKDF |
| `BLAKE2.h` / `BLAKE2.cpp` | `hash.rs` | BLAKE2b-256 hashing |
| `Hex.h` / `Hex.cpp` | `hex.rs` | Hex encoding/decoding |
| `Random.h` / `Random.cpp` | `random.rs` | CSPRNG |
| `Curve25519.h` / `Curve25519.cpp` | `curve25519.rs`, `sealed_box.rs` | ECDH and sealed box split across two modules |
| `SecretKey.h` / `SecretKey.cpp` | `keys.rs`, `signature.rs` | Key types and signing utilities |
| `StrKey.h` / `StrKey.cpp` | *(re-export)* `stellar_strkey` | StrKey encoding/decoding delegated to external crate |
| `ShortHash.h` / `ShortHash.cpp` | `short_hash.rs` | SipHash-2-4 |
| `SignerKey.h` / `SignerKey.cpp` | `signer_key.rs` | KeyFunctions<SignerKey> specialization |
| `SignerKeyUtils.h` / `SignerKeyUtils.cpp` | `signer_key.rs` | SignerKey construction utilities |
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
| `SHA256::reset()` | `Sha256Hasher::reset()` | Full |
| `SHA256::add(ByteSlice)` | `Sha256Hasher::update(&[u8])` | Full |
| `SHA256::finish()` | `Sha256Hasher::finalize()` | Full |
| `xdrSha256<T>(T)` | `xdr_sha256<T: WriteXdr>(T)` | Full |
| `hmacSha256(key, bin)` | `hmac_sha256(&[u8; 32], &[u8])` | Full |
| `hmacSha256Verify(hmac, key, bin)` | `hmac_sha256_verify(&[u8; 32], &[u8; 32], &[u8])` | Full |
| `hkdfExtract(bin)` | `hkdf_extract(&[u8])` | Full |
| `hkdfExpand(key, bin)` | `hkdf_expand(&[u8; 32], &[u8])` | Full |
| `blake2(ByteSlice)` | `blake2(&[u8])` | Full |
| `BLAKE2::BLAKE2()` | `Blake2Hasher::new()` | Full |
| `BLAKE2::reset()` | `Blake2Hasher::reset()` | Full |
| `BLAKE2::add(ByteSlice)` | `Blake2Hasher::update(&[u8])` | Full |
| `BLAKE2::finish()` | `Blake2Hasher::finalize()` | Full |
| `xdrBlake2<T>(T)` | `xdr_blake2<T: WriteXdr>(T)` | Full |

### hex.rs (`hex.rs`)

Corresponds to: `Hex.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `binToHex(ByteSlice)` | `bin_to_hex(&[u8])` | Full |
| `hexAbbrev(ByteSlice)` | `hex_abbrev(&[u8])` | Full |
| `hexToBin(string)` | `hex_to_bin(&str)` | Full |
| `hexToBin256(string)` | `hex_to_bin_256(&str)` | Full |

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
| `clearCurve25519Keys(pub, sec)` | `clear_curve25519_keys(&mut pub, &mut sec)` | Full |
| `curve25519DeriveSharedKey(sec, lpub, rpub, first)` | `Curve25519Secret::derive_shared_key(sec, lpub, rpub, first)` | Full |
| `hash<Curve25519Public>::operator()` | `Hash for Curve25519Public` | Full |

### sealed_box.rs (`sealed_box.rs`)

Corresponds to: `Curve25519.h` (encrypt/decrypt portion)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `curve25519Encrypt<N>(pub, bin)` | `seal_to_public_key(pub, &[u8])` | Full |
| `curve25519Decrypt(sec, pub, enc)` | `open_from_secret_key(sec, &[u8])` | Full |

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
| `PubKeyUtils::verifySig(key, sig, bin)` | `PublicKey::verify(&[u8], &Signature)` | Full |
| `PubKeyUtils::clearVerifySigCache()` | -- | None |
| `PubKeyUtils::seedVerifySigCache(seed)` | -- | None |
| `PubKeyUtils::flushVerifySigCacheCounts(hits, misses)` | -- | None |
| `PubKeyUtils::random()` | -- | None |
| `StrKeyUtils::logKey(ostream, key)` | -- | None |
| `HashUtils::random()` | -- | None |
| `hash<PublicKey>::operator()` | `Hash for PublicKey` (derived) | Full |

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
| `shortHash::initialize()` | `initialize()` | Full |
| `shortHash::getShortHashInitKey()` | -- | None |
| `shortHash::seed(unsigned int)` | `seed(u32)` | Full |
| `shortHash::computeHash(ByteSlice)` | `compute_hash(&[u8])` | Full |
| `shortHash::xdrComputeHash<T>(T)` | `xdr_compute_hash<T: WriteXdr>(T)` | Full |

### signer_key.rs (`signer_key.rs`)

Corresponds to: `SignerKeyUtils.h`, `SignerKey.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SignerKeyUtils::preAuthTxKey(TransactionFrame)` | `pre_auth_tx_key(&Hash256)` | Full |
| `SignerKeyUtils::preAuthTxKey(FeeBumpTransactionFrame)` | `pre_auth_tx_key(&Hash256)` | Full |
| `SignerKeyUtils::hashXKey(ByteSlice)` | `hash_x_key(&[u8])` | Full |
| `SignerKeyUtils::ed25519PayloadKey(uint256, payload)` | `ed25519_payload_key(&[u8; 32], &[u8])` | Full |
| `KeyFunctions<SignerKey>::getEd25519Value()` | `get_ed25519_from_signer_key(&SignerKey)` | Full |

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
| `XDRHasher<T>` CRTP base class | Rust uses allocation-based XDR serialization; the public `xdrSha256`/`xdrBlake2` functions are implemented differently |
| `XDRSHA256::hashBytes()` | Internal CRTP implementation detail; public API `xdrSha256<T>` is fully implemented |
| `XDRBLAKE2::hashBytes()` | Internal CRTP implementation detail; public API `xdrBlake2<T>` is fully implemented |
| `XDRShortHasher` (zero-alloc streaming) | Performance optimization; `xdr_compute_hash()` provides equivalent functionality via allocation |
| `KeyFunctions<PublicKey>` template specialization | Functionality covered by direct `PublicKey` method implementations |
| `KeyFunctions<SignerKey>` (5 of 7 methods) | `getKeyTypeName`, `getKeyVersionIsSupported`, `toKeyType`, `toKeyVersion`, `getKeyValue`, `setKeyValue` are replaced by Rust match expressions and direct construction |
| `SecretKey::benchmarkOpsPerSecond()` | Benchmark-only utility, not needed for correctness |
| `SecretKey::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard), Rust tests use `SecretKey::generate()` |
| `SecretKey::pseudoRandomForTestingFromSeed()` | Test-only (`BUILD_TESTS` guard) |
| `PubKeyUtils::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard) |
| `HashUtils::pseudoRandomForTesting()` | Test-only (`BUILD_TESTS` guard) |
| `PubKeyUtils::enableRustDalekVerify()` | Protocol migration helper; Rust implementation always uses ed25519-dalek |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `SecretKey::isZero()` | Low | Simple utility; can use `as_bytes() == &[0u8; 32]` |
| `SecretKey::operator==` | Low | Compare via `as_bytes()` but no `PartialEq` impl |
| `SecretKey::operator<` | Low | Ordering comparison for sorting |
| `PubKeyUtils::clearVerifySigCache()` | Low | Signature verification cache not implemented |
| `PubKeyUtils::seedVerifySigCache()` | Low | Signature verification cache not implemented |
| `PubKeyUtils::flushVerifySigCacheCounts()` | Low | Signature verification cache not implemented |
| `PubKeyUtils::random()` | Low | Random public key generation utility |
| `HashUtils::random()` | Low | Random hash generation utility |
| `StrKeyUtils::logKey()` | Low | Logging utility for key inspection |
| `strKey::getStrKeySize()` | Low | Computes encoded StrKey size from data size |
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
   - **stellar-core**: Maintains a process-wide `RandomEvictionCache<Hash, bool>` that caches verification results keyed by BLAKE2 hash of (key, sig, data), with hit/miss counters
   - **Rust**: Performs verification on every call without caching
   - **Rationale**: Cache is a performance optimization; can be added later if profiling shows it is needed

4. **Key type template system**
   - **stellar-core**: Uses `KeyFunctions<T>` template specializations to provide generic StrKey encode/decode for `PublicKey`, `SecretKey`, and `SignerKey`
   - **Rust**: Provides separate typed functions and dedicated methods on key types; StrKey encoding delegated to `stellar_strkey` crate
   - **Rationale**: Rust's type system and trait implementations replace the need for C++ template specializations; the specific function names are more discoverable

5. **Error handling**
   - **stellar-core**: Throws C++ exceptions (`CryptoError`, `std::runtime_error`, `InvalidStrKey`)
   - **Rust**: Returns `Result<T, CryptoError>` with typed error variants
   - **Rationale**: Idiomatic Rust error handling; all failure modes are explicit in function signatures

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Hashing (SHA256, BLAKE2) | 7 TEST_CASE (incl. 2 bench) | 22 #[test] | Rust has more granular tests |
| HMAC / HKDF | 2 TEST_CASE | 7 #[test] | Good coverage on both sides |
| Signing / Verification | 3 TEST_CASE (incl. 2 bench) | 7 #[test] | Upstream has Ed25519 test vectors |
| StrKey | 2 TEST_CASE, 5 SECTION | 13 #[test] | Good coverage on both sides |
| Short Hash | 3 TEST_CASE (incl. 2 bench) | 5 #[test] | Comparable coverage |
| Hex | Included in CryptoTests | 8 #[test] | Rust has dedicated hex tests |
| Curve25519 / ECDH | -- | 9 #[test] | Upstream tests are in overlay |
| Random | 1 TEST_CASE | 3 #[test] | Basic non-determinism checks |
| SignerKey | Included in StrKey tests | 7 #[test] | Good coverage |
| Sealed Box | -- | 8 #[test] | Upstream tests in overlay |

### Test Gaps

- **Ed25519 test vectors**: Upstream has extensive test vectors from IACR 2020/1244 and Zcash (`TEST_CASE` with many sub-cases). The Rust crate relies on `ed25519-dalek`'s own test suite for these vectors rather than replicating them.
- **StrKey edge cases**: Upstream `CryptoTests.cpp` has a large `TEST_CASE("StrKey tests")` block with many edge cases for invalid StrKeys. The Rust tests cover round-trips and basic error cases but not all upstream edge cases.
- **Benchmark tests**: Upstream has 6 benchmark `TEST_CASE` entries (SHA256, BLAKE2, ShortHash bytes/XDR, sign-and-verify, verify-hit). The Rust crate has no benchmark tests.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 52 |
| Gaps (None + Partial) | 15 |
| Intentional Omissions | 13 |
| **Parity** | **52 / (52 + 15) = 78%** |
