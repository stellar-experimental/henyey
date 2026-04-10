//! Signature weight checking for multi-signature transactions.
//!
//! This module provides the [`SignatureChecker`] type that validates transaction
//! signatures against account signers, accumulating weights to ensure the required
//! threshold is met.
//!
//! # Overview
//!
//! The signature checker implements the same logic as stellar-core's
//! `SignatureChecker` class, processing signers in a specific order:
//!
//! 1. Pre-auth TX signers (compared against transaction hash)
//! 2. Hash-X signers (preimage verification)
//! 3. Ed25519 signers (cryptographic signature verification)
//! 4. Ed25519 signed payload signers
//!
//! Each matching signature's signer weight is accumulated until the required
//! threshold is reached or all signatures are exhausted.
//!
//! # Example
//!
//! ```ignore
//! use henyey_tx::signature_checker::SignatureChecker;
//!
//! let mut checker = SignatureChecker::new(tx_hash, &signatures);
//!
//! // Check if transaction source has sufficient weight
//! if checker.check_signature(&signers, needed_threshold) {
//!     // Threshold met
//! }
//!
//! // Verify all signatures were used (no extras)
//! if !checker.check_all_signatures_used() {
//!     // Extra unused signatures present
//! }
//! ```

use henyey_common::Hash256;
use henyey_crypto::{PublicKey, Signature};
use std::collections::HashMap;
use stellar_xdr::curr::{DecoratedSignature, Signer, SignerKey, SignerKeyType};

/// Signature checker that tracks which signatures have been used and
/// accumulates weights when checking against account signers.
///
/// This implements the same logic as stellar-core's SignatureChecker class.
pub struct SignatureChecker<'a> {
    /// Hash of the transaction contents being signed.
    contents_hash: Hash256,
    /// Reference to the signatures on the transaction.
    signatures: &'a [DecoratedSignature],
    /// Tracks which signatures have been used (parallel to signatures).
    used_signatures: Vec<bool>,
}

impl<'a> SignatureChecker<'a> {
    /// Create a new signature checker.
    ///
    /// # Arguments
    ///
    /// * `contents_hash` - Transaction hash that signatures are verified against
    /// * `signatures` - Slice of decorated signatures from the transaction
    pub fn new(contents_hash: Hash256, signatures: &'a [DecoratedSignature]) -> Self {
        Self {
            contents_hash,
            signatures,
            used_signatures: vec![false; signatures.len()],
        }
    }

    /// Check if the provided signers meet the required weight threshold.
    ///
    /// Processes signers in order: PRE_AUTH_TX -> HASH_X -> ED25519 -> ED25519_SIGNED_PAYLOAD.
    /// Accumulates weights and marks signatures as used.
    ///
    /// # Arguments
    ///
    /// * `signers` - List of signers from the account (including master key as signer)
    /// * `needed_weight` - Required weight threshold to meet
    ///
    /// # Returns
    ///
    /// `true` if the accumulated weight meets or exceeds the needed weight.
    pub fn check_signature(&mut self, signers: &[Signer], needed_weight: i32) -> bool {
        // Split signers by type for ordered processing
        let mut signers_by_type = split_signers_by_type(signers);

        let mut total_weight: i32 = 0;

        // 1. Check PRE_AUTH_TX signers first (direct hash comparison)
        if let Some(pre_auth_signers) = signers_by_type.get(&SignerKeyType::PreAuthTx) {
            for signer in pre_auth_signers {
                if let SignerKey::PreAuthTx(hash) = &signer.key {
                    if hash.0 == self.contents_hash.0 {
                        let weight = self.cap_weight(signer.weight);
                        total_weight += weight as i32;
                        if total_weight >= needed_weight {
                            return true;
                        }
                    }
                }
            }
        }

        // 2. Check HASH_X signers
        if let Some(hash_x_signers) = signers_by_type.get_mut(&SignerKeyType::HashX) {
            if self.verify_all_of_type(
                hash_x_signers,
                needed_weight,
                &mut total_weight,
                verify_hash_x,
            ) {
                return true;
            }
        }

        // 3. Check ED25519 signers
        if let Some(ed25519_signers) = signers_by_type.get_mut(&SignerKeyType::Ed25519) {
            let contents_hash = self.contents_hash;
            if self.verify_all_of_type(
                ed25519_signers,
                needed_weight,
                &mut total_weight,
                |sig, signer| verify_ed25519(sig, signer, &contents_hash),
            ) {
                return true;
            }
        }

        // 4. Check ED25519_SIGNED_PAYLOAD signers
        if let Some(payload_signers) = signers_by_type.get_mut(&SignerKeyType::Ed25519SignedPayload)
        {
            if self.verify_all_of_type(
                payload_signers,
                needed_weight,
                &mut total_weight,
                verify_ed25519_signed_payload,
            ) {
                return true;
            }
        }

        false
    }

    /// Verify signatures against a list of signers of a specific type.
    ///
    /// Iterates through all signatures and checks each against the provided signers.
    /// When a match is found, the signature is marked as used, the signer's weight
    /// is accumulated, and the signer is removed from the list (each signer can
    /// only be used once).
    fn verify_all_of_type<F>(
        &mut self,
        signers: &mut Vec<Signer>,
        needed_weight: i32,
        total_weight: &mut i32,
        verify: F,
    ) -> bool
    where
        F: Fn(&DecoratedSignature, &Signer) -> bool,
    {
        for (sig_idx, sig) in self.signatures.iter().enumerate() {
            if self.used_signatures[sig_idx] {
                continue; // Already used
            }

            let mut found_idx = None;
            for (signer_idx, signer) in signers.iter().enumerate() {
                if verify(sig, signer) {
                    found_idx = Some(signer_idx);
                    break;
                }
            }

            if let Some(signer_idx) = found_idx {
                self.used_signatures[sig_idx] = true;
                let weight = self.cap_weight(signers[signer_idx].weight);
                *total_weight += weight as i32;

                if *total_weight >= needed_weight {
                    return true;
                }

                // Remove the signer so it can't be used again
                signers.remove(signer_idx);
            }
        }

        false
    }

    /// Cap weight at u8::MAX.
    ///
    /// Per the stellar-core implementation, signer weights are capped at 255
    /// to prevent overflow issues.
    fn cap_weight(&self, weight: u32) -> u32 {
        if weight > u8::MAX as u32 {
            u8::MAX as u32
        } else {
            weight
        }
    }

    /// Check if all provided signatures have been used.
    ///
    /// Returns `false` if any signature was not matched to a signer. This is
    /// used to detect extra unused signatures which is not allowed.
    ///
    /// # Returns
    ///
    /// `true` if all signatures were used, `false` if any signature is unused.
    pub fn check_all_signatures_used(&self) -> bool {
        self.used_signatures.iter().all(|&used| used)
    }

    /// Get which signatures have been used (for debugging/testing).
    pub fn used_signatures(&self) -> &[bool] {
        &self.used_signatures
    }
}

/// Split signers into groups by key type for ordered processing.
fn split_signers_by_type(signers: &[Signer]) -> HashMap<SignerKeyType, Vec<Signer>> {
    let mut result: HashMap<SignerKeyType, Vec<Signer>> = HashMap::new();

    for signer in signers {
        let key_type = match &signer.key {
            SignerKey::Ed25519(_) => SignerKeyType::Ed25519,
            SignerKey::PreAuthTx(_) => SignerKeyType::PreAuthTx,
            SignerKey::HashX(_) => SignerKeyType::HashX,
            SignerKey::Ed25519SignedPayload(_) => SignerKeyType::Ed25519SignedPayload,
        };

        result.entry(key_type).or_default().push(signer.clone());
    }

    result
}

/// Verify a HASH_X signature.
///
/// The signature is a variable-length preimage (up to XDR Signature<64> limit)
/// whose SHA-256 hash must equal the signer key's hash. Matches stellar-core's
/// verifyHashX which accepts any preimage length.
fn verify_hash_x(sig: &DecoratedSignature, signer: &Signer) -> bool {
    let SignerKey::HashX(expected_hash) = &signer.key else {
        return false;
    };

    // Check hint matches last 4 bytes of expected hash
    let expected_hint = [
        expected_hash.0[28],
        expected_hash.0[29],
        expected_hash.0[30],
        expected_hash.0[31],
    ];
    if sig.hint.0 != expected_hint {
        return false;
    }

    // Hash the preimage and compare
    let hash = Hash256::hash(&sig.signature.0);
    hash.0 == expected_hash.0
}

/// Verify an Ed25519 signature against the contents hash.
fn verify_ed25519(sig: &DecoratedSignature, signer: &Signer, contents_hash: &Hash256) -> bool {
    let SignerKey::Ed25519(key_bytes) = &signer.key else {
        return false;
    };

    // Check hint matches last 4 bytes of public key
    let expected_hint = [
        key_bytes.0[28],
        key_bytes.0[29],
        key_bytes.0[30],
        key_bytes.0[31],
    ];
    if sig.hint.0 != expected_hint {
        return false;
    }

    // Verify the cryptographic signature (decompresses public key only on cache miss)
    let Ok(signature) = Signature::try_from(&sig.signature) else {
        return false;
    };

    henyey_crypto::verify_hash_from_raw_key(&key_bytes.0, contents_hash, &signature).is_ok()
}

/// Verify an Ed25519 signed payload signature.
///
/// Per CAP-0040, the signature is verified against the raw payload bytes.
/// The hint is XOR of pubkey hint and payload hint.
fn verify_ed25519_signed_payload(sig: &DecoratedSignature, signer: &Signer) -> bool {
    let SignerKey::Ed25519SignedPayload(signed_payload) = &signer.key else {
        return false;
    };

    // The hint for signed payloads is XOR of pubkey hint and payload hint.
    // See SignatureUtils::getSignedPayloadHint in stellar-core.
    let pubkey_hint = [
        signed_payload.ed25519.0[28],
        signed_payload.ed25519.0[29],
        signed_payload.ed25519.0[30],
        signed_payload.ed25519.0[31],
    ];
    let payload_hint = if signed_payload.payload.len() >= 4 {
        let len = signed_payload.payload.len();
        [
            signed_payload.payload[len - 4],
            signed_payload.payload[len - 3],
            signed_payload.payload[len - 2],
            signed_payload.payload[len - 1],
        ]
    } else {
        // For shorter payloads, stellar-core getHint copies from the beginning
        let mut hint = [0u8; 4];
        for (i, &byte) in signed_payload.payload.iter().enumerate() {
            if i < 4 {
                hint[i] = byte;
            }
        }
        hint
    };
    let expected_hint = [
        pubkey_hint[0] ^ payload_hint[0],
        pubkey_hint[1] ^ payload_hint[1],
        pubkey_hint[2] ^ payload_hint[2],
        pubkey_hint[3] ^ payload_hint[3],
    ];

    if sig.hint.0 != expected_hint {
        return false;
    }

    let Ok(public_key) = PublicKey::from_bytes(&signed_payload.ed25519.0) else {
        return false;
    };

    let Ok(ed_sig) = Signature::try_from(&sig.signature) else {
        return false;
    };

    // stellar-core verifies the signature against the raw payload bytes,
    // not a hash. This is per CAP-0040 - the signed payload signer
    // requires a valid signature of the payload from the ed25519 public key.
    public_key.verify(&signed_payload.payload, &ed_sig).is_ok()
}

/// Collect all signers for an account including the master key.
///
/// Creates a signer list from the account's explicit signers plus the master
/// key (using the account's master weight from `thresholds[0]`).
pub fn collect_signers_for_account(account: &stellar_xdr::curr::AccountEntry) -> Vec<Signer> {
    let mut signers: Vec<Signer> = account.signers.iter().cloned().collect();

    // Add the master key as a signer with weight from thresholds[0]
    let master_weight = account.thresholds.0[0] as u32;
    if master_weight > 0 {
        // Extract the Ed25519 key bytes from the PublicKey
        let key_bytes = match &account.account_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.clone(),
        };
        signers.push(Signer {
            key: SignerKey::Ed25519(key_bytes),
            weight: master_weight,
        });
    }

    signers
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_crypto::{sign_hash, SecretKey};
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, PublicKey as XdrPublicKey,
        Signature as XdrSignature, SignatureHint, Thresholds, Uint256,
    };

    fn create_test_hash() -> Hash256 {
        Hash256([1u8; 32])
    }

    fn create_signed_signature(secret: &SecretKey, hash: &Hash256) -> DecoratedSignature {
        let signature = sign_hash(secret, hash);
        let public_key = secret.public_key();
        let pk_bytes = public_key.as_bytes();
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);

        DecoratedSignature {
            hint,
            signature: XdrSignature(signature.0.to_vec().try_into().unwrap()),
        }
    }

    fn create_signer_from_secret(secret: &SecretKey, weight: u32) -> Signer {
        Signer {
            key: SignerKey::Ed25519(Uint256(*secret.public_key().as_bytes())),
            weight,
        }
    }

    #[test]
    fn test_single_signature_meets_threshold() {
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let hash = create_test_hash();
        let sig = create_signed_signature(&secret, &hash);
        let signatures = vec![sig];

        let signer = create_signer_from_secret(&secret, 10);
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Threshold of 10 should be met with weight 10
        assert!(checker.check_signature(&signers, 10));
        assert!(checker.check_all_signatures_used());
    }

    #[test]
    fn test_single_signature_below_threshold() {
        let secret = SecretKey::from_seed(&[2u8; 32]);
        let hash = create_test_hash();
        let sig = create_signed_signature(&secret, &hash);
        let signatures = vec![sig];

        let signer = create_signer_from_secret(&secret, 5);
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Threshold of 10 should not be met with weight 5
        assert!(!checker.check_signature(&signers, 10));
    }

    #[test]
    fn test_multiple_signatures_accumulate_weight() {
        let secret1 = SecretKey::from_seed(&[3u8; 32]);
        let secret2 = SecretKey::from_seed(&[4u8; 32]);
        let hash = create_test_hash();

        let sig1 = create_signed_signature(&secret1, &hash);
        let sig2 = create_signed_signature(&secret2, &hash);
        let signatures = vec![sig1, sig2];

        let signer1 = create_signer_from_secret(&secret1, 5);
        let signer2 = create_signer_from_secret(&secret2, 5);
        let signers = vec![signer1, signer2];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Threshold of 10 should be met with 5 + 5
        assert!(checker.check_signature(&signers, 10));
        assert!(checker.check_all_signatures_used());
    }

    #[test]
    fn test_weight_cap_protocol_10_plus() {
        let secret = SecretKey::from_seed(&[5u8; 32]);
        let hash = create_test_hash();
        let sig = create_signed_signature(&secret, &hash);
        let signatures = vec![sig];

        let signer = create_signer_from_secret(&secret, 1000); // Above u8::MAX
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // With protocol 21, weight should be capped at 255
        assert!(checker.check_signature(&signers, 255));
        assert!(!checker.check_signature(&signers.clone(), 256)); // Can't meet this
    }

    #[test]
    fn test_unused_signature_detected() {
        let secret1 = SecretKey::from_seed(&[7u8; 32]);
        let secret2 = SecretKey::from_seed(&[8u8; 32]);
        let hash = create_test_hash();

        let sig1 = create_signed_signature(&secret1, &hash);
        let sig2 = create_signed_signature(&secret2, &hash);
        let signatures = vec![sig1, sig2];

        // Only provide signer for secret1
        let signer1 = create_signer_from_secret(&secret1, 10);
        let signers = vec![signer1];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Threshold is met
        assert!(checker.check_signature(&signers, 10));
        // But not all signatures were used (sig2 is unused)
        assert!(!checker.check_all_signatures_used());
    }

    #[test]
    fn test_pre_auth_tx_signer() {
        let hash = create_test_hash();
        let signatures = vec![]; // No signatures needed for pre-auth

        let signer = Signer {
            key: SignerKey::PreAuthTx(Uint256(hash.0)),
            weight: 10,
        };
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Pre-auth TX matches the hash directly
        assert!(checker.check_signature(&signers, 10));
    }

    #[test]
    fn test_hash_x_signer() {
        // Create a preimage
        let preimage = [42u8; 32];
        let hash_of_preimage = Hash256::hash(&preimage);

        // Create a signature that is the preimage
        let sig = DecoratedSignature {
            hint: SignatureHint([
                hash_of_preimage.0[28],
                hash_of_preimage.0[29],
                hash_of_preimage.0[30],
                hash_of_preimage.0[31],
            ]),
            signature: XdrSignature(preimage.to_vec().try_into().unwrap()),
        };
        let signatures = vec![sig];

        let signer = Signer {
            key: SignerKey::HashX(Uint256(hash_of_preimage.0)),
            weight: 10,
        };
        let signers = vec![signer];

        let tx_hash = create_test_hash(); // Irrelevant for HashX
        let mut checker = SignatureChecker::new(tx_hash, &signatures);

        assert!(checker.check_signature(&signers, 10));
        assert!(checker.check_all_signatures_used());
    }

    #[test]
    fn test_collect_signers_for_account() {
        let master_key_bytes = Uint256([10u8; 32]);
        let extra_signer = Signer {
            key: SignerKey::Ed25519(Uint256([20u8; 32])),
            weight: 5,
        };

        let account = AccountEntry {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(master_key_bytes.clone())),
            balance: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: Thresholds([10, 1, 2, 3]), // Master weight = 10
            signers: vec![extra_signer.clone()].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        };

        let signers = collect_signers_for_account(&account);

        // Should have the extra signer plus the master key
        assert_eq!(signers.len(), 2);

        // Find master key signer
        let master_signer = signers
            .iter()
            .find(|s| matches!(&s.key, SignerKey::Ed25519(k) if k.0 == master_key_bytes.0));
        assert_eq!(master_signer.expect("expected Some").weight, 10);
    }

    #[test]
    fn test_signer_used_only_once() {
        let secret = SecretKey::from_seed(&[9u8; 32]);
        let hash = create_test_hash();

        // Create two identical signatures
        let sig1 = create_signed_signature(&secret, &hash);
        let sig2 = create_signed_signature(&secret, &hash);
        let signatures = vec![sig1, sig2];

        let signer = create_signer_from_secret(&secret, 5);
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Only weight 5 should be counted (signer used once)
        assert!(!checker.check_signature(&signers, 10));
        // Second signature should be unused
        assert!(!checker.check_all_signatures_used());
    }

    /// Test that zero master weight doesn't add master as signer.
    #[test]
    fn test_collect_signers_zero_master_weight() {
        let master_key_bytes = Uint256([11u8; 32]);

        let account = AccountEntry {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(master_key_bytes.clone())),
            balance: 1000,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: Thresholds([0, 1, 2, 3]), // Master weight = 0
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        };

        let signers = collect_signers_for_account(&account);

        // No signers when master weight is 0 and no extra signers
        assert_eq!(signers.len(), 0);
    }

    /// Test hash_x with wrong hint fails.
    #[test]
    fn test_hash_x_wrong_hint() {
        let preimage = [42u8; 32];
        let hash_of_preimage = Hash256::hash(&preimage);

        // Create a signature with wrong hint
        let sig = DecoratedSignature {
            hint: SignatureHint([0, 0, 0, 0]), // Wrong hint
            signature: XdrSignature(preimage.to_vec().try_into().unwrap()),
        };
        let signatures = vec![sig];

        let signer = Signer {
            key: SignerKey::HashX(Uint256(hash_of_preimage.0)),
            weight: 10,
        };
        let signers = vec![signer];

        let tx_hash = create_test_hash();
        let mut checker = SignatureChecker::new(tx_hash, &signatures);

        // Should fail due to wrong hint
        assert!(!checker.check_signature(&signers, 10));
    }

    /// Test hash_x with wrong preimage fails.
    #[test]
    fn test_hash_x_wrong_preimage() {
        let correct_preimage = [42u8; 32];
        let wrong_preimage = [99u8; 32];
        let hash_of_correct = Hash256::hash(&correct_preimage);

        // Create a signature with wrong preimage but correct hint
        let sig = DecoratedSignature {
            hint: SignatureHint([
                hash_of_correct.0[28],
                hash_of_correct.0[29],
                hash_of_correct.0[30],
                hash_of_correct.0[31],
            ]),
            signature: XdrSignature(wrong_preimage.to_vec().try_into().unwrap()),
        };
        let signatures = vec![sig];

        let signer = Signer {
            key: SignerKey::HashX(Uint256(hash_of_correct.0)),
            weight: 10,
        };
        let signers = vec![signer];

        let tx_hash = create_test_hash();
        let mut checker = SignatureChecker::new(tx_hash, &signatures);

        // Should fail due to wrong preimage
        assert!(!checker.check_signature(&signers, 10));
    }

    /// Test ed25519 with wrong hint fails.
    #[test]
    fn test_ed25519_wrong_hint() {
        let secret = SecretKey::from_seed(&[12u8; 32]);
        let hash = create_test_hash();
        let mut sig = create_signed_signature(&secret, &hash);
        sig.hint = SignatureHint([0, 0, 0, 0]); // Wrong hint

        let signatures = vec![sig];

        let signer = create_signer_from_secret(&secret, 10);
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Should fail due to wrong hint
        assert!(!checker.check_signature(&signers, 10));
    }

    /// Test empty signatures list.
    #[test]
    fn test_empty_signatures() {
        let hash = create_test_hash();
        let signatures: Vec<DecoratedSignature> = vec![];

        let secret = SecretKey::from_seed(&[13u8; 32]);
        let signer = create_signer_from_secret(&secret, 10);
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // No signatures means threshold can't be met
        assert!(!checker.check_signature(&signers, 10));
        assert!(checker.check_all_signatures_used()); // No signatures to use
    }

    /// Test that Hash-X accepts variable-length preimages (not just 32 bytes).
    /// Regression test for AUDIT-723: verify_hash_x rejected valid non-32-byte
    /// preimages with a hard 32-byte length check. Stellar-core accepts any
    /// preimage length (up to XDR Signature<64> limit) as long as SHA-256(preimage)
    /// matches the signer key hash.
    #[test]
    fn test_audit_723_hash_x_accepts_variable_length_preimage() {
        // Use a 16-byte preimage (shorter than 32)
        let short_preimage = vec![0xAA; 16];
        let hash_of_short = Hash256::hash(&short_preimage);

        let sig = DecoratedSignature {
            hint: SignatureHint([
                hash_of_short.0[28],
                hash_of_short.0[29],
                hash_of_short.0[30],
                hash_of_short.0[31],
            ]),
            signature: XdrSignature(short_preimage.try_into().unwrap()),
        };
        let signatures = vec![sig];

        let signer = Signer {
            key: SignerKey::HashX(Uint256(hash_of_short.0)),
            weight: 10,
        };
        let signers = vec![signer];

        let tx_hash = create_test_hash();
        let mut checker = SignatureChecker::new(tx_hash, &signatures);

        // A 16-byte preimage with correct hash should be accepted
        assert!(
            checker.check_signature(&signers, 10),
            "Hash-X should accept variable-length preimages"
        );

        // Also test a 64-byte preimage (maximum XDR Signature length)
        let long_preimage = vec![0xBB; 64];
        let hash_of_long = Hash256::hash(&long_preimage);

        let sig2 = DecoratedSignature {
            hint: SignatureHint([
                hash_of_long.0[28],
                hash_of_long.0[29],
                hash_of_long.0[30],
                hash_of_long.0[31],
            ]),
            signature: XdrSignature(long_preimage.try_into().unwrap()),
        };
        let signatures2 = vec![sig2];

        let signer2 = Signer {
            key: SignerKey::HashX(Uint256(hash_of_long.0)),
            weight: 10,
        };
        let signers2 = vec![signer2];

        let mut checker2 = SignatureChecker::new(tx_hash, &signatures2);
        assert!(
            checker2.check_signature(&signers2, 10),
            "Hash-X should accept 64-byte preimages"
        );
    }

    /// Test pre_auth_tx signer doesn't match wrong hash.
    #[test]
    fn test_pre_auth_tx_wrong_hash() {
        let hash = create_test_hash();
        let wrong_hash = Hash256([99u8; 32]);
        let signatures = vec![];

        let signer = Signer {
            key: SignerKey::PreAuthTx(Uint256(wrong_hash.0)), // Different hash
            weight: 10,
        };
        let signers = vec![signer];

        let mut checker = SignatureChecker::new(hash, &signatures);

        // Pre-auth TX doesn't match
        assert!(!checker.check_signature(&signers, 10));
    }
}
