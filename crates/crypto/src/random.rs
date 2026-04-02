//! Cryptographically secure random number generation.
//!
//! This module provides functions for generating random bytes and integers
//! using the operating system's cryptographic random number generator
//! ([`OsRng`](rand::rngs::OsRng)).
//!
//! All functions in this module are suitable for cryptographic use, including:
//! - Key generation
//! - Nonce/IV generation
//! - Random challenges
//!
//! # Example
//!
//! ```ignore
//! use henyey_crypto::{random_bytes, random_u64, fill_random};
//!
//! // Generate a fixed-size random array
//! let key: [u8; 32] = random_bytes();
//!
//! // Generate a random integer
//! let nonce = random_u64();
//!
//! // Fill an existing buffer with random data
//! let mut buffer = [0u8; 64];
//! fill_random(&mut buffer);
//! ```

use rand::{rngs::OsRng, RngCore};

/// Generates a fixed-size array of cryptographically secure random bytes.
///
/// The size is determined by the const generic parameter `N`.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::random_bytes;
///
/// let key: [u8; 32] = random_bytes();
/// let nonce: [u8; 24] = random_bytes();
/// ```
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Generates a random 64-bit unsigned integer.
#[cfg(test)]
fn random_u64() -> u64 {
    OsRng.next_u64()
}

/// Fills a mutable slice with cryptographically secure random bytes.
///
/// This is useful when you need to fill a dynamically-sized buffer.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::fill_random;
///
/// let mut buffer = vec![0u8; 128];
/// fill_random(&mut buffer);
/// ```
#[cfg(test)]
fn fill_random(dest: &mut [u8]) {
    OsRng.fill_bytes(dest);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_bytes() {
        let a: [u8; 32] = random_bytes();
        let b: [u8; 32] = random_bytes();

        // Should produce different values (with overwhelming probability)
        assert_ne!(a, b);
    }

    #[test]
    fn test_random_u64() {
        let a = random_u64();
        let b = random_u64();

        // Should produce different values (with overwhelming probability)
        assert_ne!(a, b);
    }

    #[test]
    fn test_fill_random() {
        let mut buf = [0u8; 32];
        fill_random(&mut buf);

        // Should not be all zeros
        assert_ne!(buf, [0u8; 32]);
    }
}
