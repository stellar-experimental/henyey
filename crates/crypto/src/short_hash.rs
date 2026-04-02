//! SipHash-2-4 short hashing for deterministic ordering.
//!
//! This module provides a process-global SipHash-2-4 hasher used for
//! deterministic ordering of ledger entries in bucket lists and other
//! data structures that require consistent ordering across nodes.
//!
//! # Why SipHash?
//!
//! SipHash-2-4 provides:
//! - Fast hashing suitable for hash tables and ordering
//! - Protection against hash-flooding DoS attacks
//! - Deterministic output given the same key
//!
//! # Global Key Management
//!
//! The short hash key is initialized once per process:
//!
//! - By default, a random key is generated on first use
//! - For tests or replay, [`seed`] can set a deterministic key
//! - Once hashing begins, the key cannot be changed (to ensure consistency)
//!
//! # Thread Safety
//!
//! All functions in this module are thread-safe. The global key state is
//! protected by a mutex.
//!
//! # Example
//!
//! ```
//! use henyey_crypto::{compute_hash, initialize};
//!
//! // Initialize with a random key (optional, happens automatically)
//! initialize();
//!
//! // Compute a short hash
//! let hash = compute_hash(b"some data");
//! ```

use crate::random;
use crate::CryptoError;
use siphasher::sip::SipHasher24;
use std::hash::Hasher;
use std::sync::{Mutex, OnceLock};
use stellar_xdr::curr::{Limits, WriteXdr};

/// Size of the SipHash key in bytes (128 bits).
const KEY_BYTES: usize = 16;

/// Internal state for the global short hash key.
struct KeyState {
    /// The 128-bit SipHash key.
    key: [u8; KEY_BYTES],
    /// Whether any hashing has occurred (prevents reseeding).
    have_hashed: bool,
    /// The explicit seed value if set via [`seed`], or 0 for random.
    explicit_seed: u32,
}

impl KeyState {
    /// Creates a new key state with a random key.
    fn new() -> Self {
        Self {
            key: random::random_bytes(),
            have_hashed: false,
            explicit_seed: 0,
        }
    }
}

/// Returns a reference to the global key state.
fn key_state() -> &'static Mutex<KeyState> {
    static STATE: OnceLock<Mutex<KeyState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(KeyState::new()))
}

/// Initializes the short hash key with fresh random bytes.
///
/// This is optional; the key is automatically initialized with random bytes
/// on first use. Call this to explicitly reinitialize (e.g., at startup).
///
/// # Panics
///
/// Panics if the internal mutex is poisoned.
pub fn initialize() {
    let mut state = key_state().lock().expect("short hash lock poisoned");
    state.key = random::random_bytes();
}

/// Expands a 32-bit seed to a 128-bit SipHash key by repeating the byte pattern.
fn expand_seed_to_key(seed: u32) -> [u8; KEY_BYTES] {
    let mut key = [0u8; KEY_BYTES];
    for (i, byte) in key.iter_mut().enumerate() {
        let shift = i % std::mem::size_of::<u32>();
        *byte = (seed >> shift) as u8;
    }
    key
}

fn compute_hash_with_key(key: &[u8; KEY_BYTES], bytes: &[u8]) -> u64 {
    let mut hasher = SipHasher24::new_with_key(key);
    hasher.write(bytes);
    hasher.finish()
}

/// Seeds the short hash key with a deterministic value.
///
/// This is used for tests and replay scenarios where deterministic ordering
/// is required. The seed is expanded to a 128-bit key.
///
/// # Errors
///
/// Returns [`CryptoError::ShortHashSeedConflict`] if:
/// - Hashing has already occurred with a different seed
/// - The key was initialized randomly and hashing has begun
///
/// Calling `seed` multiple times with the same value is allowed.
///
/// # Panics
///
/// Panics if the internal mutex is poisoned.
pub fn seed(seed: u32) -> Result<(), CryptoError> {
    let mut state = key_state().lock().expect("short hash lock poisoned");
    if state.have_hashed && state.explicit_seed != seed {
        return Err(CryptoError::ShortHashSeedConflict {
            existing: state.explicit_seed,
            requested: seed,
        });
    }
    state.explicit_seed = seed;
    state.key = expand_seed_to_key(seed);
    Ok(())
}

/// Computes a SipHash-2-4 hash of raw bytes.
///
/// This uses the process-global key. Once this function is called, the key
/// is locked and cannot be reseeded with a different value.
///
/// # Panics
///
/// Panics if the internal mutex is poisoned.
pub fn compute_hash(bytes: &[u8]) -> u64 {
    let mut state = key_state().lock().expect("short hash lock poisoned");
    state.have_hashed = true;
    compute_hash_with_key(&state.key, bytes)
}

/// Computes a SipHash-2-4 hash of an XDR-encoded value.
///
/// The value is first serialized to XDR bytes, then hashed. This ensures
/// consistent hashing of XDR types across the codebase.
///
/// # Errors
///
/// Returns [`CryptoError::Xdr`] if XDR serialization fails.
///
/// # Panics
///
/// Panics if the internal mutex is poisoned.
pub fn xdr_compute_hash<T: WriteXdr>(value: &T) -> Result<u64, CryptoError> {
    let bytes = value.to_xdr(Limits::none())?;
    Ok(compute_hash(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use stellar_xdr::curr::LedgerEntry;

    fn test_guard() -> MutexGuard<'static, ()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("test guard poisoned")
    }

    fn reset_state() {
        let mut state = key_state().lock().expect("short hash lock poisoned");
        *state = KeyState::new();
    }

    #[test]
    fn test_siphash_vector() {
        let _guard = test_guard();
        reset_state();
        let key: [u8; KEY_BYTES] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let msg = [0u8; 0];
        let got = compute_hash_with_key(&key, &msg);
        let expected = 0x726fdb47dd0e0e31u64;
        assert_eq!(got, expected);
    }

    #[test]
    fn test_xdr_hash_matches_bytes() {
        let _guard = test_guard();
        reset_state();
        initialize();
        let entry = LedgerEntry::default();
        let bytes = entry.to_xdr(Limits::none()).unwrap();
        let bytes_hash = compute_hash(&bytes);
        let xdr_hash = xdr_compute_hash(&entry).unwrap();
        assert_eq!(bytes_hash, xdr_hash);
    }

    #[test]
    fn test_seed_matches_stellar_core_key_derivation() {
        let _guard = test_guard();
        reset_state();
        let seed_value = 0x12345678;
        seed(seed_value).expect("seed");
        let key = expand_seed_to_key(seed_value);
        let expected = compute_hash_with_key(&key, b"");
        let got = compute_hash(b"");
        assert_eq!(got, expected);
    }

    #[test]
    fn test_seed_conflict_after_hash() {
        let _guard = test_guard();
        reset_state();
        initialize();
        let _ = compute_hash(b"warmup");
        let err = seed(1).expect_err("seed should fail");
        match err {
            CryptoError::ShortHashSeedConflict {
                existing,
                requested,
            } => {
                assert_eq!(existing, 0);
                assert_eq!(requested, 1);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn test_seed_repeat_is_allowed() {
        let _guard = test_guard();
        reset_state();
        seed(99).expect("seed");
        let _ = compute_hash(b"first");
        seed(99).expect("repeat seed");
    }
}
