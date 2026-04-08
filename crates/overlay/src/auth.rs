//! Authentication for Stellar overlay connections.
//!
//! This module implements the Stellar overlay authentication protocol, which uses
//! X25519 key exchange with HMAC-SHA256 message authentication. The protocol ensures
//! that:
//!
//! - Both peers prove ownership of their Ed25519 identity keys
//! - All messages after handshake are authenticated to prevent tampering
//! - Messages cannot be replayed (sequence numbers prevent replay attacks)
//!
//! # Handshake Protocol
//!
//! The handshake follows this sequence:
//!
//! 1. **Hello Exchange**: Both peers send `Hello` messages containing:
//!    - Their Ed25519 public key (node identity)
//!    - An ephemeral X25519 public key for key exchange
//!    - An authentication certificate (signature over the ephemeral key)
//!    - A random nonce for key derivation
//!
//! 2. **Key Derivation**: Both peers:
//!    - Verify the peer's auth certificate signature
//!    - Perform X25519 Diffie-Hellman to derive a shared secret
//!    - Use HKDF to derive separate MAC keys for each direction
//!
//! 3. **Auth Exchange**: Both peers send `Auth` messages with a valid MAC
//!    to prove they derived the correct keys
//!
//! 4. **Authenticated Channel**: All subsequent messages include:
//!    - A sequence number (prevents replay)
//!    - An HMAC-SHA256 over the sequence and message content
//!
//! # Key Types
//!
//! - [`AuthCert`] - Authentication certificate containing ephemeral key and signature
//! - [`AuthContext`] - Manages handshake state and message authentication
//! - [`AuthState`] - Current state of the authentication handshake

use crate::{LocalNode, OverlayError, PeerId, Result};
use henyey_common::Hash256;
use henyey_crypto::PublicKey;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use stellar_xdr::curr::{
    self as xdr, AuthenticatedMessage, AuthenticatedMessageV0, Curve25519Public, EnvelopeType,
    Hello, HmacSha256Key, HmacSha256Mac, StellarMessage, Uint256, WriteXdr,
};

/// Type alias for the XDR `AuthCert` — the overlay authentication certificate.
///
/// See [`AuthCertExt`] for helper methods (construction, signing, verification).
pub type AuthCert = xdr::AuthCert;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, SharedSecret};

type HmacSha256 = Hmac<Sha256>;

/// Extension trait for `AuthCert` providing construction, signing, and verification.
///
/// An `AuthCert` binds an ephemeral X25519 public key to a node's Ed25519 identity.
/// The signature proves that the owner of the Ed25519 key generated this ephemeral key,
/// preventing man-in-the-middle attacks during key exchange.
///
/// The certificate has a limited lifetime (typically 1 hour) to limit the window
/// for potential key compromise.
///
/// # Signature Format
///
/// The signature is computed over the SHA-256 hash of:
/// ```text
/// network_id (32 bytes) || ENVELOPE_TYPE_AUTH (4 bytes, big-endian) ||
/// expiration (8 bytes, big-endian) || ephemeral_pubkey (32 bytes)
/// ```
pub trait AuthCertExt {
    /// Creates a new authentication certificate.
    ///
    /// Generates a signature binding the ephemeral X25519 key to the local node's
    /// Ed25519 identity. The certificate expires 1 hour from creation.
    fn new_cert(local_node: &LocalNode, ephemeral_secret: &EphemeralSecret) -> AuthCert;

    /// Verifies this certificate was signed by the given peer.
    ///
    /// Checks that the certificate has not expired and the signature is valid.
    fn verify(
        &self,
        network_id: &henyey_common::NetworkId,
        peer_public_key: &PublicKey,
    ) -> Result<()>;
}

/// Constant-time byte array comparison to prevent timing side-channel attacks.
///
/// Returns `true` if `a` and `b` are equal, without leaking information about
/// which bytes differ through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl AuthCertExt for AuthCert {
    fn new_cert(local_node: &LocalNode, ephemeral_secret: &EphemeralSecret) -> AuthCert {
        let ephemeral_public = X25519PublicKey::from(ephemeral_secret);
        let pubkey = *ephemeral_public.as_bytes();

        // Expiration: 1 hour from now.
        // Matches stellar-core `expirationLimit` in PeerAuth.cpp.
        const AUTH_CERT_EXPIRATION_SECS: u64 = 3600;
        let expiration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + AUTH_CERT_EXPIRATION_SECS;

        // Sign: network_id || ENVELOPE_TYPE_AUTH || expiration || pubkey
        let sig = sign_cert(local_node, expiration, &pubkey);

        AuthCert {
            pubkey: Curve25519Public { key: pubkey },
            expiration,
            sig: xdr::Signature(sig.to_vec().try_into().unwrap()),
        }
    }

    fn verify(
        &self,
        network_id: &henyey_common::NetworkId,
        peer_public_key: &PublicKey,
    ) -> Result<()> {
        // Check expiration
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if self.expiration <= now {
            return Err(OverlayError::AuthenticationFailed(
                "auth cert expired".to_string(),
            ));
        }

        // Reconstruct the signed data
        let mut data = Vec::with_capacity(32 + 4 + 8 + 32);
        data.extend_from_slice(network_id.as_bytes());
        data.extend_from_slice(&(EnvelopeType::Auth as i32).to_be_bytes());
        data.extend_from_slice(&self.expiration.to_be_bytes());
        data.extend_from_slice(&self.pubkey.key);

        // Verify signature over the hash (matching stellar-core's approach)
        let hash = Hash256::hash(&data);
        let sig_bytes: [u8; 64] = self.sig.0.as_slice().try_into().map_err(|_| {
            OverlayError::AuthenticationFailed("invalid signature length".to_string())
        })?;
        let sig = henyey_crypto::Signature::from_bytes(sig_bytes);
        peer_public_key.verify(hash.as_bytes(), &sig).map_err(|_| {
            OverlayError::AuthenticationFailed("invalid auth cert signature".to_string())
        })
    }
}

/// Signs the certificate data using the local node's Ed25519 key.
///
/// Following stellar-core's implementation, we sign the SHA-256 hash of
/// the concatenated certificate fields, not the raw data.
fn sign_cert(local_node: &LocalNode, expiration: u64, pubkey: &[u8; 32]) -> [u8; 64] {
    let mut data = Vec::with_capacity(32 + 4 + 8 + 32);
    data.extend_from_slice(local_node.network_id.as_bytes());
    data.extend_from_slice(&(EnvelopeType::Auth as i32).to_be_bytes());
    data.extend_from_slice(&expiration.to_be_bytes());
    data.extend_from_slice(pubkey);

    // stellar-core signs the SHA-256 hash of the data, not the raw data
    let hash = Hash256::hash(&data);
    let signature = local_node.secret_key.sign(hash.as_bytes());
    *signature.as_bytes()
}

/// Current state of the authentication handshake.
///
/// Tracks the progress of the Hello/Auth message exchange between peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    /// Initial state - no messages exchanged yet.
    Initial,
    /// We have sent our Hello, waiting for peer's Hello.
    HelloSent,
    /// We have received peer's Hello, ready to send Auth.
    HelloReceived,
    /// We have sent Auth, waiting for peer's Auth.
    AuthSent,
    /// Handshake complete - both peers have exchanged Hello and Auth.
    Authenticated,
    /// Authentication failed due to an error.
    Failed,
}

/// Authentication context for a peer connection.
///
/// Manages the complete authentication handshake lifecycle:
///
/// 1. Generates ephemeral keys and authentication certificates
/// 2. Processes incoming Hello messages and verifies peer certificates
/// 3. Derives MAC keys using HKDF from the X25519 shared secret
/// 4. Wraps outgoing messages with sequence numbers and MACs
/// 5. Unwraps and verifies incoming messages
///
/// # Key Derivation
///
/// The MAC keys are derived using HKDF (RFC 5869):
/// - Extract: `PRK = HMAC-SHA256(salt=zeros, IKM=shared_secret || A_pub || B_pub)`
/// - Expand: `key = HMAC-SHA256(PRK, prefix || nonce1 || nonce2 || 0x01)`
///
/// Where A is the initiator (outbound) and B is the acceptor (inbound).
/// Each direction has a separate key derived with different prefixes.
pub struct AuthContext {
    /// Local node identity and configuration.
    local_node: LocalNode,

    /// Our ephemeral X25519 secret key (consumed during key exchange).
    our_ephemeral_secret: Option<EphemeralSecret>,

    /// Our ephemeral X25519 public key.
    our_ephemeral_public: Option<X25519PublicKey>,

    /// Our authentication certificate.
    our_auth_cert: Option<AuthCert>,

    /// Random nonce we sent in our Hello message.
    our_nonce: [u8; 32],

    /// Nonce received from peer's Hello message.
    peer_nonce: Option<[u8; 32]>,

    /// Peer's authentication certificate.
    peer_auth_cert: Option<AuthCert>,

    /// Peer's Ed25519 public key (identity).
    peer_public_key: Option<PublicKey>,

    /// Peer's node ID (derived from public key).
    peer_id: Option<PeerId>,

    /// X25519 shared secret (result of Diffie-Hellman).
    shared_secret: Option<SharedSecret>,

    /// HMAC key for messages we send.
    send_mac_key: Option<HmacSha256Key>,

    /// HMAC key for messages we receive.
    recv_mac_key: Option<HmacSha256Key>,

    /// Next sequence number for outgoing messages.
    send_sequence: u64,

    /// Expected sequence number for incoming messages.
    recv_sequence: u64,

    /// Current state of the handshake.
    state: AuthState,

    /// True if we initiated the connection (outbound).
    we_called_remote: bool,
}

impl AuthContext {
    /// Creates a new authentication context for a connection.
    ///
    /// Generates ephemeral keys and prepares the authentication certificate.
    /// The `we_called_remote` parameter determines which role we play in
    /// key derivation (initiator vs acceptor).
    ///
    /// # Arguments
    ///
    /// * `local_node` - Our node's identity and configuration
    /// * `we_called_remote` - True if we initiated the connection (outbound)
    pub fn new(local_node: LocalNode, we_called_remote: bool) -> Self {
        // Generate ephemeral key pair
        let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);
        let auth_cert = AuthCert::new_cert(&local_node, &ephemeral_secret);

        // Generate our nonce for Hello message
        let our_nonce = rand::random::<[u8; 32]>();

        Self {
            local_node,
            our_ephemeral_secret: Some(ephemeral_secret),
            our_ephemeral_public: Some(ephemeral_public),
            our_auth_cert: Some(auth_cert),
            our_nonce,
            peer_nonce: None,
            peer_auth_cert: None,
            peer_public_key: None,
            peer_id: None,
            shared_secret: None,
            send_mac_key: None,
            recv_mac_key: None,
            send_sequence: 0,
            recv_sequence: 0,
            state: AuthState::Initial,
            we_called_remote,
        }
    }

    /// Returns the current authentication state.
    pub fn state(&self) -> AuthState {
        self.state
    }

    /// Returns true if the handshake is complete and the channel is authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.state == AuthState::Authenticated
    }

    /// Returns the peer's ID if the Hello has been processed.
    pub fn peer_id(&self) -> Option<&PeerId> {
        self.peer_id.as_ref()
    }

    /// Returns the local node's peer ID.
    pub fn local_peer_id(&self) -> PeerId {
        PeerId::from_xdr(self.local_node.xdr_public_key())
    }

    /// Creates a Hello message to send to the peer.
    ///
    /// The Hello message contains our identity, protocol versions, and
    /// authentication certificate with ephemeral key.
    pub fn create_hello(&self) -> Hello {
        let public_key = self.local_node.xdr_public_key();

        Hello {
            ledger_version: self.local_node.ledger_version,
            overlay_version: self.local_node.overlay_version,
            overlay_min_version: self.local_node.overlay_min_version,
            network_id: self.local_node.network_id.into(),
            version_str: self
                .local_node
                .version_string
                .clone()
                .try_into()
                .unwrap_or_default(),
            listening_port: self.local_node.listening_port as i32,
            peer_id: xdr::NodeId(public_key),
            cert: self.our_auth_cert.as_ref().unwrap().clone(),
            nonce: Uint256(self.our_nonce),
        }
    }

    /// Processes a received Hello message from the peer.
    ///
    /// This is the core of the authentication handshake. It:
    /// 1. Verifies the peer is on the same network
    /// 2. Checks protocol version compatibility
    /// 3. Verifies the peer's authentication certificate
    /// 4. Performs X25519 key exchange to derive the shared secret
    /// 5. Derives separate MAC keys for each direction
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Network ID doesn't match (wrong network)
    /// - Peer's overlay version is below our minimum
    /// - Auth certificate signature is invalid or expired
    /// - Key derivation fails
    pub fn process_hello(&mut self, hello: &Hello) -> Result<()> {
        // Check network ID
        let network_id_bytes = hello.network_id.0;
        if network_id_bytes != *self.local_node.network_id.as_bytes() {
            return Err(OverlayError::NetworkMismatch);
        }

        // Check overlay version range compatibility (matching stellar-core Peer::recvHello)
        // Three conditions that cause version rejection:
        // 1. Peer's range is malformed (min > max)
        // 2. Peer's max version is below our minimum (peer too old)
        // 3. Peer's min version is above our maximum (peer too new)
        if hello.overlay_min_version > hello.overlay_version {
            return Err(OverlayError::VersionMismatch(format!(
                "peer overlay version range malformed: min {} > max {}",
                hello.overlay_min_version, hello.overlay_version
            )));
        }
        if hello.overlay_version < self.local_node.overlay_min_version {
            return Err(OverlayError::VersionMismatch(format!(
                "peer overlay version {} below minimum {}",
                hello.overlay_version, self.local_node.overlay_min_version
            )));
        }
        if hello.overlay_min_version > self.local_node.overlay_version {
            return Err(OverlayError::VersionMismatch(format!(
                "peer overlay min version {} above our maximum {}",
                hello.overlay_min_version, self.local_node.overlay_version
            )));
        }

        // Extract peer public key
        let peer_pk_bytes = match &hello.peer_id.0 {
            xdr::PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => *bytes,
        };
        let peer_public_key = PublicKey::from_bytes(&peer_pk_bytes).map_err(|e| {
            OverlayError::AuthenticationFailed(format!("invalid peer public key: {}", e))
        })?;

        // Verify auth cert (hello.cert is already the XDR AuthCert type)
        let peer_auth_cert = hello.cert.clone();
        peer_auth_cert.verify(&self.local_node.network_id, &peer_public_key)?;

        // Store peer's nonce
        self.peer_nonce = Some(hello.nonce.0);

        // Perform X25519 key exchange
        let our_secret = self.our_ephemeral_secret.take().ok_or_else(|| {
            OverlayError::AuthenticationFailed("ephemeral secret already used".to_string())
        })?;
        let peer_ephemeral_public = X25519PublicKey::from(peer_auth_cert.pubkey.key);
        let shared_secret = our_secret.diffie_hellman(&peer_ephemeral_public);

        // Reject small-order public keys that produce an all-zeros shared secret.
        // stellar-core: Curve25519.cpp:60-63 checks crypto_scalarmult() return code,
        // which libsodium uses to reject these points.
        if shared_secret.as_bytes() == &[0u8; 32] {
            return Err(OverlayError::AuthenticationFailed(
                "small-order public key produced all-zeros shared secret".to_string(),
            ));
        }

        // Derive MAC keys using nonces from Hello messages
        let (send_key, recv_key) = self.derive_mac_keys(&shared_secret, &peer_auth_cert)?;

        // Store peer info
        self.peer_public_key = Some(peer_public_key);
        self.peer_id = Some(PeerId::from_bytes(peer_pk_bytes));
        self.peer_auth_cert = Some(peer_auth_cert);
        self.shared_secret = Some(shared_secret);
        self.send_mac_key = Some(send_key);
        self.recv_mac_key = Some(recv_key);
        self.state = AuthState::HelloReceived;

        Ok(())
    }

    /// Derives MAC keys from the shared secret using HKDF.
    ///
    /// Follows stellar-core's key derivation scheme:
    ///
    /// 1. **Extract phase**: `PRK = HKDF_extract(ECDH_result || A_pub || B_pub)`
    ///    where A is the initiator and B is the acceptor
    ///
    /// 2. **Expand phase** for each direction:
    ///    - `SendKey = HKDF_expand(PRK, prefix || local_nonce || peer_nonce)`
    ///    - `RecvKey = HKDF_expand(PRK, prefix || peer_nonce || local_nonce)`
    ///
    /// The prefix is 0 for A->B messages and 1 for B->A messages, ensuring
    /// each direction has a unique key.
    fn derive_mac_keys(
        &self,
        shared_secret: &SharedSecret,
        peer_auth_cert: &AuthCert,
    ) -> Result<(HmacSha256Key, HmacSha256Key)> {
        let our_public = self.our_ephemeral_public.as_ref().ok_or_else(|| {
            OverlayError::AuthenticationFailed("ephemeral public key missing".to_string())
        })?;
        let peer_nonce = self
            .peer_nonce
            .as_ref()
            .ok_or_else(|| OverlayError::AuthenticationFailed("peer nonce missing".to_string()))?;

        // Step 1: HKDF-Extract to create shared key K
        // IKM = ECDH_result || A_pub || B_pub (where A is initiator, B is acceptor)
        let (a_pub, b_pub) = if self.we_called_remote {
            (our_public.as_bytes(), &peer_auth_cert.pubkey.key)
        } else {
            (&peer_auth_cert.pubkey.key, our_public.as_bytes())
        };

        let mut ikm = Vec::with_capacity(32 + 32 + 32);
        ikm.extend_from_slice(shared_secret.as_bytes());
        ikm.extend_from_slice(a_pub);
        ikm.extend_from_slice(b_pub);

        // HKDF-Extract: PRK = HMAC-SHA256(salt="", IKM)
        // With empty salt, HMAC uses a key of all zeros
        let zero_salt = [0u8; 32];
        let mut extract_mac = HmacSha256::new_from_slice(&zero_salt)
            .map_err(|_| OverlayError::AuthenticationFailed("HMAC init failed".to_string()))?;
        extract_mac.update(&ikm);
        let prk: [u8; 32] = extract_mac.finalize().into_bytes().into();

        // Step 2: Determine prefixes based on role
        // Prefix 0 is for A's messages (A→B direction)
        // Prefix 1 is for B's messages (B→A direction)
        let (send_prefix, recv_prefix): (u8, u8) = if self.we_called_remote {
            // We are A: we send A→B (prefix 0), receive B→A (prefix 1)
            (0, 1)
        } else {
            // We are B: we send B→A (prefix 1), receive A→B (prefix 0)
            (1, 0)
        };

        // Step 3: HKDF-Expand for each direction
        // Send: prefix || local_nonce || remote_nonce
        // Recv: prefix || remote_nonce || local_nonce
        let send_key = self.hkdf_expand(&prk, send_prefix, &self.our_nonce, peer_nonce);
        let recv_key = self.hkdf_expand(&prk, recv_prefix, peer_nonce, &self.our_nonce);

        Ok((send_key, recv_key))
    }

    /// HKDF-Expand: derives a MAC key from the PRK using prefix and nonces.
    ///
    /// Computes `T(1) = HMAC-SHA256(PRK, info || 0x01)` where
    /// `info = prefix || nonce1 || nonce2`.
    fn hkdf_expand(
        &self,
        prk: &[u8; 32],
        prefix: u8,
        nonce1: &[u8; 32],
        nonce2: &[u8; 32],
    ) -> HmacSha256Key {
        // info = prefix || nonce1 || nonce2
        let mut info = Vec::with_capacity(1 + 32 + 32);
        info.push(prefix);
        info.extend_from_slice(nonce1);
        info.extend_from_slice(nonce2);

        // HKDF-Expand: T(1) = HMAC-Hash(PRK, info || 0x01)
        let mut expand_mac = HmacSha256::new_from_slice(prk).unwrap();
        expand_mac.update(&info);
        expand_mac.update(&[0x01]);
        let key: [u8; 32] = expand_mac.finalize().into_bytes().into();

        HmacSha256Key { key }
    }

    /// Marks that we have sent our Hello message.
    ///
    /// Call this after successfully sending the Hello to update state tracking.
    pub fn hello_sent(&mut self) {
        // For the initiator, this transitions Initial → HelloSent.
        // For the responder (who receives Hello first), state is already
        // HelloReceived so this is a no-op — that's correct per OVERLAY_SPEC §4.2.
        if self.state == AuthState::Initial {
            self.state = AuthState::HelloSent;
        }
    }

    /// Marks that we have sent our Auth message.
    ///
    /// Call this after successfully sending the Auth to update state tracking.
    /// For the responder, Auth is sent after receiving the peer's Auth, so the
    /// state may already be Authenticated — the no-op is intentional.
    pub fn auth_sent(&mut self) {
        if self.state == AuthState::HelloReceived {
            self.state = AuthState::AuthSent;
        }
    }

    /// Processes a received Auth message, completing the handshake.
    ///
    /// After this succeeds, the connection is fully authenticated and
    /// all messages will be verified with MACs.
    pub fn process_auth(&mut self) -> Result<()> {
        // AUTH messages consume sequence 0 on both sides
        // So first post-auth messages use sequence 1
        self.recv_sequence = 1;
        self.send_sequence = 1;

        // Mark as authenticated
        self.state = AuthState::Authenticated;
        Ok(())
    }

    /// Wraps a message with sequence number and MAC for sending.
    ///
    /// This should only be called after authentication is complete.
    /// Each call increments the sequence number to prevent replay attacks.
    ///
    /// # Errors
    ///
    /// Returns an error if the send MAC key is not established.
    pub fn wrap_message(&mut self, message: StellarMessage) -> Result<AuthenticatedMessage> {
        let send_key = self.send_mac_key.as_ref().ok_or_else(|| {
            OverlayError::AuthenticationFailed("send key not established".to_string())
        })?;

        let sequence = self.send_sequence;
        self.send_sequence += 1;

        // Compute MAC
        let mac = self.compute_mac(send_key, sequence, &message)?;

        Ok(AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence,
            message,
            mac,
        }))
    }

    /// Unwraps and verifies a received message.
    ///
    /// Checks the sequence number and MAC to ensure the message is authentic
    /// and has not been replayed. MAC verification is gated on the receiver's
    /// own authentication state (not a wire flag) to prevent attackers from
    /// bypassing MAC checks by manipulating the length prefix.
    ///
    /// # Arguments
    ///
    /// * `auth_msg` - The authenticated message wrapper from the wire
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The sequence number doesn't match the expected value
    /// - The MAC verification fails
    pub fn unwrap_message(&mut self, auth_msg: AuthenticatedMessage) -> Result<StellarMessage> {
        match auth_msg {
            AuthenticatedMessage::V0(v0) => {
                let msg_type = crate::codec::helpers::message_type_name(&v0.message);
                tracing::debug!(
                    "unwrap_message: seq={}, is_auth={}, expected_seq={}, type={}",
                    v0.sequence,
                    self.is_authenticated(),
                    self.recv_sequence,
                    msg_type
                );

                // Gate MAC verification on whether MAC keys are available,
                // matching stellar-core's `getState(guard) >= GOT_HELLO`
                // check in Peer.cpp:1032. MAC keys are derived during the
                // Hello exchange (process_hello), so verification covers
                // both the Auth message and all post-Auth messages.
                // Error messages are exempt (they can use sequence 0 and skip MAC).
                let is_error = matches!(v0.message, StellarMessage::ErrorMsg(_));
                if self.recv_mac_key.is_some() && !is_error {
                    // Verify sequence number
                    if v0.sequence != self.recv_sequence {
                        return Err(OverlayError::AuthenticationFailed(format!(
                            "sequence mismatch: expected {}, got {}",
                            self.recv_sequence, v0.sequence
                        )));
                    }
                    self.recv_sequence += 1;

                    // Verify MAC
                    let recv_key = self.recv_mac_key.as_ref().ok_or_else(|| {
                        OverlayError::AuthenticationFailed("recv key not established".to_string())
                    })?;

                    let expected_mac = self.compute_mac(recv_key, v0.sequence, &v0.message)?;
                    tracing::debug!(
                        "MAC verification: seq={}, expected={:02x?}, got={:02x?}",
                        v0.sequence,
                        &expected_mac.mac[..8],
                        &v0.mac.mac[..8],
                    );
                    // Spec: OVERLAY_SPEC §3.4 — MAC comparison MUST be constant-time
                    // to prevent timing side-channel attacks.
                    if !constant_time_eq(&expected_mac.mac, &v0.mac.mac) {
                        return Err(OverlayError::MacVerificationFailed);
                    }
                }

                Ok(v0.message)
            }
        }
    }

    /// Computes the HMAC-SHA256 for a message.
    ///
    /// The MAC is computed over: `sequence (8 bytes, big-endian) || message_xdr`
    fn compute_mac(
        &self,
        key: &HmacSha256Key,
        sequence: u64,
        message: &StellarMessage,
    ) -> Result<HmacSha256Mac> {
        let message_bytes = message.to_xdr(xdr::Limits::none())?;

        let mut mac = HmacSha256::new_from_slice(&key.key)
            .map_err(|_| OverlayError::AuthenticationFailed("invalid MAC key".to_string()))?;

        mac.update(&sequence.to_be_bytes());
        mac.update(&message_bytes);

        let result = mac.finalize().into_bytes();
        let mut mac_bytes = [0u8; 32];
        mac_bytes.copy_from_slice(&result);

        Ok(HmacSha256Mac { mac: mac_bytes })
    }

    /// Wraps a Hello message without MAC authentication.
    ///
    /// Hello messages are sent before keys are established, so they use
    /// sequence 0 and an all-zero MAC field.
    pub fn wrap_unauthenticated(&self, message: StellarMessage) -> AuthenticatedMessage {
        // Hello message uses sequence 0 and zero MAC
        AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message,
            mac: HmacSha256Mac { mac: [0u8; 32] },
        })
    }

    /// Wraps an Auth message with MAC but sequence 0.
    ///
    /// Auth messages are special: they use sequence 0 (like Hello) but include
    /// a valid MAC to prove we derived the correct keys. This proves to the peer
    /// that we successfully completed the key exchange.
    pub fn wrap_auth_message(&self, message: StellarMessage) -> Result<AuthenticatedMessage> {
        let send_key = self.send_mac_key.as_ref().ok_or_else(|| {
            OverlayError::AuthenticationFailed("send key not established".to_string())
        })?;

        // Auth message uses sequence 0
        let sequence = 0u64;

        // Compute MAC
        let mac = self.compute_mac(send_key, sequence, &message)?;

        Ok(AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence,
            message,
            mac,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_crypto::SecretKey;

    #[test]
    fn test_auth_cert_creation() {
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let cert = AuthCert::new_cert(&local_node, &ephemeral);

        // Verify it with our own public key
        let result = cert.verify(&local_node.network_id, &local_node.public_key());
        assert!(result.is_ok(), "Self-verification should pass");
    }

    #[test]
    fn test_auth_context_creation() {
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let ctx = AuthContext::new(local_node, true);

        assert_eq!(ctx.state(), AuthState::Initial);
        assert!(!ctx.is_authenticated());
    }

    #[test]
    fn test_hello_creation() {
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let ctx = AuthContext::new(local_node, true);

        let hello = ctx.create_hello();
        assert_eq!(hello.overlay_version, 40);
        assert_eq!(hello.overlay_min_version, 38);
        assert_eq!(hello.ledger_version, 26);
    }

    // ---- G14: Auth flag (bit 31) gates MAC verification in unwrap_message ----

    /// Helper: Complete a full handshake between two AuthContexts.
    /// Returns (initiator, acceptor) both in Authenticated state.
    fn complete_handshake() -> (AuthContext, AuthContext) {
        let secret_a = SecretKey::generate();
        let secret_b = SecretKey::generate();
        let node_a = LocalNode::new_testnet(secret_a);
        let node_b = LocalNode::new_testnet(secret_b);

        let mut ctx_a = AuthContext::new(node_a, true); // initiator
        let mut ctx_b = AuthContext::new(node_b, false); // acceptor

        // Exchange Hello messages
        let hello_a = ctx_a.create_hello();
        let hello_b = ctx_b.create_hello();

        ctx_a.hello_sent();
        ctx_b.hello_sent();

        ctx_b
            .process_hello(&hello_a)
            .expect("B should accept A's hello");
        ctx_a
            .process_hello(&hello_b)
            .expect("A should accept B's hello");

        // Exchange Auth messages
        ctx_a.auth_sent();
        ctx_b.auth_sent();
        ctx_a.process_auth().expect("A should complete auth");
        ctx_b.process_auth().expect("B should complete auth");

        assert!(ctx_a.is_authenticated());
        assert!(ctx_b.is_authenticated());

        (ctx_a, ctx_b)
    }

    #[test]
    fn test_unwrap_authenticated_message_verifies_mac_g14() {
        // After handshake, wrapping and unwrapping should succeed with valid MAC.
        let (mut ctx_a, mut ctx_b) = complete_handshake();

        let msg = StellarMessage::Peers(xdr::VecM::default());
        let wrapped = ctx_a.wrap_message(msg.clone()).unwrap();

        // message_is_authenticated=true → MAC should be verified
        let unwrapped = ctx_b.unwrap_message(wrapped).unwrap();
        assert!(matches!(unwrapped, StellarMessage::Peers(_)));
    }

    #[test]
    fn test_unwrap_bad_mac_rejected_when_authenticated_g14() {
        // After handshake, a message with a wrong MAC should be rejected
        // when bit 31 is set (message_is_authenticated=true).
        let (mut ctx_a, mut ctx_b) = complete_handshake();

        let msg = StellarMessage::Peers(xdr::VecM::default());
        let wrapped = ctx_a.wrap_message(msg).unwrap();

        // Tamper with the MAC
        let tampered = match wrapped {
            AuthenticatedMessage::V0(mut v0) => {
                v0.mac.mac[0] ^= 0xff;
                AuthenticatedMessage::V0(v0)
            }
        };

        let result = ctx_b.unwrap_message(tampered);
        assert!(
            result.is_err(),
            "tampered MAC should be rejected when is_authenticated=true"
        );
        assert!(
            matches!(result, Err(OverlayError::MacVerificationFailed)),
            "error should be MacVerificationFailed"
        );
    }

    #[test]
    fn test_unwrap_skips_mac_when_not_authenticated_g14() {
        // Before handshake completes, MAC is not checked because
        // the receiver's auth state is not yet authenticated.
        // This is the correct behavior matching stellar-core's
        // `getState(guard) >= GOT_HELLO` check.
        let secret = SecretKey::generate();
        let node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(node, true);

        assert!(!ctx.is_authenticated());

        // Create a message with garbage MAC
        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Hello(Default::default()),
            mac: HmacSha256Mac { mac: [0xAA; 32] }, // garbage MAC
        });

        // Pre-auth: MAC should not be verified because ctx is not authenticated
        let result = ctx.unwrap_message(msg);
        assert!(
            result.is_ok(),
            "MAC check should be skipped before authentication is complete"
        );
    }

    #[test]
    fn test_unwrap_skips_mac_before_authentication_g14() {
        // Before handshake completes (ctx.is_authenticated() == false),
        // MAC verification is skipped regardless, since the receiver's
        // own auth state gates the check.
        let secret = SecretKey::generate();
        let node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(node, true);

        assert!(!ctx.is_authenticated());

        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Hello(Default::default()),
            mac: HmacSha256Mac { mac: [0xBB; 32] }, // garbage MAC
        });

        // Pre-auth messages should not be MAC-checked
        let result = ctx.unwrap_message(msg);
        assert!(
            result.is_ok(),
            "pre-auth messages should not be MAC-checked"
        );
    }

    #[test]
    fn test_unwrap_sequence_mismatch_rejected_g14() {
        // After handshake, a message with wrong sequence number should be rejected.
        let (mut ctx_a, mut ctx_b) = complete_handshake();

        let msg = StellarMessage::Peers(xdr::VecM::default());
        let wrapped = ctx_a.wrap_message(msg).unwrap();

        // Tamper with the sequence number
        let tampered = match wrapped {
            AuthenticatedMessage::V0(mut v0) => {
                v0.sequence = 999; // wrong sequence
                AuthenticatedMessage::V0(v0)
            }
        };

        let result = ctx_b.unwrap_message(tampered);
        assert!(result.is_err(), "wrong sequence should be rejected");
        match result {
            Err(OverlayError::AuthenticationFailed(msg)) => {
                assert!(
                    msg.contains("sequence mismatch"),
                    "error should mention sequence: {}",
                    msg
                );
            }
            other => panic!("expected AuthenticationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_unwrap_error_msg_skips_mac_g14() {
        // ErrorMsg messages skip MAC verification even after authentication,
        // matching stellar-core behavior where errors can use sequence 0.
        let (mut _ctx_a, mut ctx_b) = complete_handshake();

        let error_msg = StellarMessage::ErrorMsg(xdr::SError {
            code: xdr::ErrorCode::Misc,
            msg: "test error".try_into().unwrap_or_default(),
        });

        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: error_msg,
            mac: HmacSha256Mac { mac: [0; 32] }, // zero MAC (invalid)
        });

        // Error messages should bypass MAC check
        let result = ctx_b.unwrap_message(msg);
        assert!(
            result.is_ok(),
            "error messages should skip MAC verification"
        );
    }

    #[test]
    fn test_wrap_unwrap_roundtrip_multiple_messages_g14() {
        // Verify that multiple messages can be wrapped and unwrapped in sequence,
        // with sequence numbers incrementing correctly.
        let (mut ctx_a, mut ctx_b) = complete_handshake();

        for i in 0..5 {
            let msg = StellarMessage::Peers(xdr::VecM::default());
            let wrapped = ctx_a.wrap_message(msg).unwrap();

            // Verify sequence increments
            match &wrapped {
                AuthenticatedMessage::V0(v0) => {
                    assert_eq!(
                        v0.sequence,
                        i + 1,
                        "send sequence should start at 1 and increment"
                    );
                }
            }

            let unwrapped = ctx_b.unwrap_message(wrapped).unwrap();
            assert!(matches!(unwrapped, StellarMessage::Peers(_)));
        }
    }

    #[test]
    fn test_audit_c1_mac_bypass_rejected() {
        // Regression test for AUDIT-C1: An attacker clears bit 31 in the
        // length prefix to set message_is_authenticated=false, attempting
        // to bypass MAC verification on an authenticated connection.
        //
        // After handshake, MAC verification must be gated on the receiver's
        // own auth state (self.is_authenticated()), NOT the wire flag.
        // A forged message with garbage MAC must be rejected even when the
        // wire flag says "not authenticated".
        let (mut ctx_a, mut ctx_b) = complete_handshake();

        // ctx_a wraps a legitimate message (this advances send sequence)
        let msg = StellarMessage::Peers(xdr::VecM::default());
        let wrapped = ctx_a.wrap_message(msg).unwrap();

        // Tamper: replace the MAC with garbage, simulating an attacker-crafted message
        let tampered = match wrapped {
            AuthenticatedMessage::V0(mut v0) => {
                v0.mac = HmacSha256Mac { mac: [0xFF; 32] };
                AuthenticatedMessage::V0(v0)
            }
        };

        // The attacker clears bit 31, so message_is_authenticated=false.
        // The current buggy code will skip MAC verification and accept this.
        // The fixed code must reject it because ctx_b.is_authenticated() is true.
        let result = ctx_b.unwrap_message(tampered);
        assert!(
            result.is_err(),
            "MAC verification must not be bypassed by clearing the wire auth flag"
        );
    }

    // ── OVERLAY_SPEC §4.2: Auth state transitions ─────────────────────

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(&[], &[]));
        assert!(constant_time_eq(&[0u8; 32], &[0u8; 32]));
    }

    #[test]
    fn test_constant_time_eq_unequal() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(&[0u8; 32], &[1u8; 32]));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn test_constant_time_eq_single_bit_difference() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[31] = 1; // differ in only the last byte's lowest bit
        assert!(!constant_time_eq(&a, &b));
    }

    #[test]
    fn test_hello_sent_initiator_transitions_to_hello_sent() {
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(local_node, true);

        assert_eq!(ctx.state(), AuthState::Initial);
        ctx.hello_sent();
        assert_eq!(ctx.state(), AuthState::HelloSent);
    }

    #[test]
    fn test_hello_sent_noop_when_already_hello_received() {
        // Responder receives Hello first, transitions to HelloReceived.
        // Calling hello_sent() afterwards should be a no-op.
        let secret_a = SecretKey::generate();
        let secret_b = SecretKey::generate();
        let node_a = LocalNode::new_testnet(secret_a);
        let node_b = LocalNode::new_testnet(secret_b);

        let ctx_a = AuthContext::new(node_a, true);
        let mut ctx_b = AuthContext::new(node_b, false);

        let hello_a = ctx_a.create_hello();
        ctx_b.process_hello(&hello_a).unwrap();
        assert_eq!(ctx_b.state(), AuthState::HelloReceived);

        // hello_sent should be a no-op
        ctx_b.hello_sent();
        assert_eq!(
            ctx_b.state(),
            AuthState::HelloReceived,
            "hello_sent must be a no-op when already HelloReceived"
        );
    }

    #[test]
    fn test_auth_sent_transitions_from_hello_received() {
        let secret_a = SecretKey::generate();
        let secret_b = SecretKey::generate();
        let node_a = LocalNode::new_testnet(secret_a);
        let node_b = LocalNode::new_testnet(secret_b);

        let ctx_a = AuthContext::new(node_a, true);
        let mut ctx_b = AuthContext::new(node_b, false);

        let hello_a = ctx_a.create_hello();
        ctx_b.process_hello(&hello_a).unwrap();
        assert_eq!(ctx_b.state(), AuthState::HelloReceived);

        ctx_b.auth_sent();
        assert_eq!(ctx_b.state(), AuthState::AuthSent);
    }

    #[test]
    fn test_auth_sent_noop_when_not_hello_received() {
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(local_node, true);

        // In Initial state, auth_sent should be a no-op
        ctx.auth_sent();
        assert_eq!(
            ctx.state(),
            AuthState::Initial,
            "auth_sent must be a no-op when not in HelloReceived state"
        );

        // In HelloSent state, also a no-op
        ctx.hello_sent();
        assert_eq!(ctx.state(), AuthState::HelloSent);
        ctx.auth_sent();
        assert_eq!(
            ctx.state(),
            AuthState::HelloSent,
            "auth_sent must be a no-op when in HelloSent state"
        );
    }

    #[test]
    fn test_auth_cert_xdr_roundtrip() {
        use stellar_xdr::curr::ReadXdr;

        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let cert = AuthCert::new_cert(&local_node, &ephemeral);

        // Serialize to XDR bytes and deserialize back
        let bytes = cert.to_xdr(xdr::Limits::none()).unwrap();
        let restored = AuthCert::from_xdr(bytes.as_slice(), xdr::Limits::none()).unwrap();
        assert!(restored
            .verify(&local_node.network_id, &local_node.public_key())
            .is_ok());
    }

    /// Helper: create a Hello message with custom overlay version fields.
    fn make_hello_with_versions(
        overlay_version: u32,
        overlay_min_version: u32,
        local_node: &LocalNode,
    ) -> xdr::Hello {
        let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let cert = AuthCert::new_cert(local_node, &ephemeral);
        xdr::Hello {
            ledger_version: 25,
            overlay_version,
            overlay_min_version,
            network_id: xdr::Hash(*local_node.network_id.as_bytes()),
            version_str: local_node.version_string.clone().try_into().unwrap(),
            listening_port: local_node.listening_port as i32,
            peer_id: xdr::NodeId(local_node.xdr_public_key()),
            cert,
            nonce: xdr::Uint256([42u8; 32]),
        }
    }

    #[test]
    fn test_process_hello_rejects_peer_too_old() {
        // Peer advertises overlay_version below our minimum (38)
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(local_node.clone(), true);
        ctx.hello_sent();

        let hello = make_hello_with_versions(37, 35, &local_node);
        let result = ctx.process_hello(&hello);
        assert!(result.is_err());
        assert!(
            matches!(&result, Err(OverlayError::VersionMismatch(msg)) if msg.contains("below minimum")),
            "expected VersionMismatch(below minimum), got: {:?}",
            result
        );
    }

    #[test]
    fn test_process_hello_rejects_peer_too_new() {
        // Peer's min version is above our max (40)
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(local_node.clone(), true);
        ctx.hello_sent();

        let hello = make_hello_with_versions(43, 41, &local_node);
        let result = ctx.process_hello(&hello);
        assert!(result.is_err());
        assert!(
            matches!(&result, Err(OverlayError::VersionMismatch(msg)) if msg.contains("above our maximum")),
            "expected VersionMismatch(above our maximum), got: {:?}",
            result
        );
    }

    #[test]
    fn test_process_hello_rejects_malformed_version_range() {
        // Peer's min > max (malformed)
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut ctx = AuthContext::new(local_node.clone(), true);
        ctx.hello_sent();

        let hello = make_hello_with_versions(38, 40, &local_node);
        let result = ctx.process_hello(&hello);
        assert!(result.is_err());
        assert!(
            matches!(&result, Err(OverlayError::VersionMismatch(msg)) if msg.contains("malformed")),
            "expected VersionMismatch(malformed), got: {:?}",
            result
        );
    }

    #[test]
    fn test_process_hello_accepts_compatible_peer() {
        // Peer with exact same range as ours (38-39)
        let secret_a = SecretKey::generate();
        let secret_b = SecretKey::generate();
        let node_a = LocalNode::new_testnet(secret_a);
        let node_b = LocalNode::new_testnet(secret_b);

        let mut ctx_a = AuthContext::new(node_a, true);
        ctx_a.hello_sent();

        // Use node_b's identity to create a properly signed Hello
        let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let cert = AuthCert::new_cert(&node_b, &ephemeral);
        let hello = xdr::Hello {
            ledger_version: 25,
            overlay_version: 39,
            overlay_min_version: 38,
            network_id: xdr::Hash(*node_b.network_id.as_bytes()),
            version_str: node_b.version_string.clone().try_into().unwrap(),
            listening_port: node_b.listening_port as i32,
            peer_id: xdr::NodeId(node_b.xdr_public_key()),
            cert,
            nonce: xdr::Uint256([42u8; 32]),
        };
        let result = ctx_a.process_hello(&hello);
        assert!(
            result.is_ok(),
            "compatible peer should be accepted: {:?}",
            result
        );
    }

    /// Helper: Complete Hello exchange only (MAC keys derived, state = HelloReceived).
    /// Does NOT complete Auth — both contexts are pre-authenticated.
    fn complete_hello_exchange() -> (AuthContext, AuthContext) {
        let secret_a = SecretKey::generate();
        let secret_b = SecretKey::generate();
        let node_a = LocalNode::new_testnet(secret_a);
        let node_b = LocalNode::new_testnet(secret_b);

        let mut ctx_a = AuthContext::new(node_a, true); // initiator
        let mut ctx_b = AuthContext::new(node_b, false); // acceptor

        let hello_a = ctx_a.create_hello();
        let hello_b = ctx_b.create_hello();

        ctx_a.hello_sent();
        ctx_b.hello_sent();

        ctx_b.process_hello(&hello_a).expect("B accepts A's hello");
        ctx_a.process_hello(&hello_b).expect("A accepts B's hello");

        assert_eq!(ctx_a.state(), AuthState::HelloReceived);
        assert_eq!(ctx_b.state(), AuthState::HelloReceived);
        assert!(ctx_a.recv_mac_key.is_some(), "MAC keys should be derived");
        assert!(ctx_b.recv_mac_key.is_some(), "MAC keys should be derived");

        (ctx_a, ctx_b)
    }

    #[test]
    fn test_audit_004_auth_msg_mac_must_be_verified() {
        // Regression test for AUDIT-004: After the Hello exchange, MAC keys
        // are derived and available, so the Auth message's MAC MUST be
        // verified. stellar-core gates MAC verification on `>= GOT_HELLO`
        // (Peer.cpp:1032), which includes the Auth receive phase.
        //
        // The bug: unwrap_message gates on is_authenticated() (state ==
        // Authenticated), but Auth arrives when state is HelloReceived.
        // This means a corrupted MAC on the Auth message is silently accepted.
        let (ctx_a, mut ctx_b) = complete_hello_exchange();

        // Wrap an Auth message with a valid MAC
        let auth_msg = StellarMessage::Auth(xdr::Auth { flags: 200 });
        let wrapped = ctx_a
            .wrap_auth_message(auth_msg)
            .expect("wrap should succeed");

        // Corrupt every MAC byte
        let corrupted = match wrapped {
            AuthenticatedMessage::V0(mut v0) => {
                for byte in v0.mac.mac.iter_mut() {
                    *byte ^= 0xFF;
                }
                AuthenticatedMessage::V0(v0)
            }
        };

        // The corrupted Auth message MUST be rejected.
        // Before the fix: this passes (MAC not checked) — test FAILS.
        // After the fix: this returns MacVerificationFailed — test PASSES.
        let result = ctx_b.unwrap_message(corrupted);
        assert!(
            result.is_err(),
            "Auth message with corrupted MAC must be rejected when MAC keys are available"
        );
        assert!(
            matches!(result, Err(OverlayError::MacVerificationFailed)),
            "expected MacVerificationFailed, got: {:?}",
            result
        );
    }
}
