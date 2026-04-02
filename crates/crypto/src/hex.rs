//! Hex encoding and decoding utilities.
//!
//! This module provides hex encoding and decoding functions compatible with
//! the stellar-core `Hex.h/.cpp` implementation.
//!
//! # Functions
//!
//! - [`bin_to_hex`]: Encode bytes as lowercase hex string
//! - [`hex_abbrev`]: Get a 6-character hex prefix (for logging)
//! - [`hex_to_bin`]: Decode hex string to bytes
//! - [`hex_to_bin_256`]: Decode hex string to exactly 32 bytes
//!
//! # Example
//!
//! ```ignore
//! use henyey_crypto::hex::{bin_to_hex, hex_abbrev, hex_to_bin, hex_to_bin_256};
//!
//! let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
//!
//! // Encode to hex
//! let hex_str = bin_to_hex(&bytes);
//! assert_eq!(hex_str, "0123456789abcdef");
//!
//! // Get abbreviated form for logging
//! let abbrev = hex_abbrev(&bytes);
//! assert_eq!(abbrev, "012345");
//!
//! // Decode back to bytes
//! let decoded = hex_to_bin(&hex_str).unwrap();
//! assert_eq!(decoded, bytes);
//! ```

#[cfg(test)]
use crate::CryptoError;

/// Hex-encode bytes as a lowercase hex string.
///
/// This is equivalent to stellar-core `binToHex()`.
///
/// # Arguments
///
/// * `data` - The bytes to encode
///
/// # Returns
///
/// A lowercase hex string representation of the bytes.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::hex::bin_to_hex;
///
/// assert_eq!(bin_to_hex(&[0x00, 0xff, 0x10]), "00ff10");
/// assert_eq!(bin_to_hex(&[]), "");
/// ```
#[inline]
#[cfg(test)]
fn bin_to_hex(data: &[u8]) -> String {
    hex::encode(data)
}

/// Get a 6-character hex prefix of the data.
///
/// This is equivalent to stellar-core `hexAbbrev()` and is used for logging
/// to show a short identifier without the full hex string.
///
/// # Arguments
///
/// * `data` - The bytes to abbreviate
///
/// # Returns
///
/// A 6-character lowercase hex string (or less if input is < 3 bytes).
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::hex::hex_abbrev;
///
/// assert_eq!(hex_abbrev(&[0x01, 0x23, 0x45, 0x67]), "012345");
/// assert_eq!(hex_abbrev(&[0xab, 0xcd]), "abcd");
/// assert_eq!(hex_abbrev(&[]), "");
/// ```
#[cfg(test)]
fn hex_abbrev(data: &[u8]) -> String {
    // Only encode up to 3 bytes (producing up to 6 hex characters)
    let prefix_len = data.len().min(3);
    bin_to_hex(&data[..prefix_len])
}

/// Decode a hex string to bytes.
///
/// This is equivalent to stellar-core `hexToBin()`.
///
/// # Arguments
///
/// * `hex_str` - A hex string (case-insensitive)
///
/// # Returns
///
/// The decoded bytes, or an error if the input is not valid hex.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::hex::hex_to_bin;
///
/// assert_eq!(hex_to_bin("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
/// assert_eq!(hex_to_bin("ABCD").unwrap(), vec![0xab, 0xcd]);
/// assert!(hex_to_bin("xyz").is_err());
/// ```
#[cfg(test)]
fn hex_to_bin(hex_str: &str) -> Result<Vec<u8>, CryptoError> {
    hex::decode(hex_str).map_err(|_| CryptoError::InvalidHex)
}

/// Decode a hex string to exactly 32 bytes.
///
/// This is equivalent to stellar-core `hexToBin256()`.
///
/// # Arguments
///
/// * `hex_str` - A 64-character hex string (case-insensitive)
///
/// # Returns
///
/// A 32-byte array, or an error if the input is not valid hex or not 32 bytes.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::hex::hex_to_bin_256;
///
/// let hex = "0000000000000000000000000000000000000000000000000000000000000000";
/// let result = hex_to_bin_256(hex).unwrap();
/// assert_eq!(result, [0u8; 32]);
///
/// // Invalid length
/// assert!(hex_to_bin_256("00").is_err());
/// ```
#[cfg(test)]
fn hex_to_bin_256(hex_str: &str) -> Result<[u8; 32], CryptoError> {
    let bytes = hex_to_bin(hex_str)?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| CryptoError::InvalidLength {
            expected: 32,
            got: v.len(),
        })
}

/// Decode a hex string to a `Hash256`.
///
/// This is a convenience wrapper around [`hex_to_bin_256`] that returns
/// a `Hash256` type.
///
/// # Arguments
///
/// * `hex_str` - A 64-character hex string (case-insensitive)
///
/// # Returns
///
/// A `Hash256`, or an error if the input is not valid hex or not 32 bytes.
///
/// # Example
///
/// ```ignore
/// use henyey_crypto::hex::hex_to_hash256;
///
/// let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// let hash = hex_to_hash256(hex).unwrap();
/// assert_eq!(hash.to_hex(), hex);
/// ```
#[cfg(test)]
fn hex_to_hash256(hex_str: &str) -> Result<henyey_common::Hash256, CryptoError> {
    let bytes = hex_to_bin_256(hex_str)?;
    Ok(henyey_common::Hash256::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bin_to_hex() {
        assert_eq!(bin_to_hex(&[]), "");
        assert_eq!(bin_to_hex(&[0x00]), "00");
        assert_eq!(bin_to_hex(&[0xff]), "ff");
        assert_eq!(bin_to_hex(&[0x01, 0x23, 0x45, 0x67]), "01234567");
        assert_eq!(bin_to_hex(&[0xab, 0xcd, 0xef]), "abcdef");
    }

    #[test]
    fn test_hex_abbrev() {
        assert_eq!(hex_abbrev(&[]), "");
        assert_eq!(hex_abbrev(&[0x12]), "12");
        assert_eq!(hex_abbrev(&[0x12, 0x34]), "1234");
        assert_eq!(hex_abbrev(&[0x12, 0x34, 0x56]), "123456");
        assert_eq!(hex_abbrev(&[0x12, 0x34, 0x56, 0x78]), "123456");
        assert_eq!(hex_abbrev(&[0x12, 0x34, 0x56, 0x78, 0x9a]), "123456");
    }

    #[test]
    fn test_hex_to_bin() {
        assert_eq!(hex_to_bin("").unwrap(), Vec::<u8>::new());
        assert_eq!(hex_to_bin("00").unwrap(), vec![0x00]);
        assert_eq!(hex_to_bin("ff").unwrap(), vec![0xff]);
        assert_eq!(hex_to_bin("FF").unwrap(), vec![0xff]);
        assert_eq!(
            hex_to_bin("0123456789abcdef").unwrap(),
            vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
        );
        assert_eq!(hex_to_bin("ABCDEF").unwrap(), vec![0xab, 0xcd, 0xef]);
    }

    #[test]
    fn test_hex_to_bin_invalid() {
        assert!(hex_to_bin("gg").is_err());
        assert!(hex_to_bin("xyz").is_err());
        assert!(hex_to_bin("0").is_err()); // Odd length
        assert!(hex_to_bin("0g").is_err());
    }

    #[test]
    fn test_hex_to_bin_256() {
        let all_zeros = "0000000000000000000000000000000000000000000000000000000000000000";
        assert_eq!(hex_to_bin_256(all_zeros).unwrap(), [0u8; 32]);

        let all_ff = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert_eq!(hex_to_bin_256(all_ff).unwrap(), [0xffu8; 32]);

        // Invalid length
        assert!(hex_to_bin_256("00").is_err());
        assert!(hex_to_bin_256("00000000").is_err());

        // Too long
        let too_long = "000000000000000000000000000000000000000000000000000000000000000000";
        assert!(hex_to_bin_256(too_long).is_err());
    }

    #[test]
    fn test_hex_to_hash256() {
        // SHA-256 of empty string
        let sha256_empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let hash = hex_to_hash256(sha256_empty).unwrap();
        assert_eq!(hash.to_hex(), sha256_empty);
    }

    #[test]
    fn test_roundtrip() {
        let original = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let hex_str = bin_to_hex(&original);
        let decoded = hex_to_bin(&hex_str).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_roundtrip_256() {
        let mut original = [0u8; 32];
        for (i, byte) in original.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let hex_str = bin_to_hex(&original);
        let decoded = hex_to_bin_256(&hex_str).unwrap();
        assert_eq!(original, decoded);
    }
}
