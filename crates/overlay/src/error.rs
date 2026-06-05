//! Error types for overlay operations.
//!
//! Defines the [`OverlayError`] enum which covers all error conditions that
//! can occur during overlay network operations, including:
//!
//! - Connection failures and timeouts
//! - Authentication and MAC verification errors
//! - Protocol version mismatches
//! - Peer management errors
//! - Internal errors

use thiserror::Error;

/// Errors that can occur during overlay network operations.
///
/// This enum covers all error conditions from connection establishment
/// through message exchange and peer management.
#[derive(Debug, Error)]
pub enum OverlayError {
    // ===== Connection Errors =====
    /// TCP connection could not be established.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    /// Connection attempt or receive operation timed out.
    #[error("connection timeout: {0}")]
    ConnectionTimeout(String),

    /// The peer closed the connection.
    #[error("peer disconnected: {0}")]
    PeerDisconnected(String),

    // ===== Authentication Errors =====
    /// Authentication handshake failed (invalid cert, bad signature, etc.).
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    /// Authentication handshake did not complete in time.
    #[error("authentication timeout")]
    AuthenticationTimeout,

    /// HMAC verification failed on a received message.
    ///
    /// This indicates either a bug, network corruption, or an attack.
    #[error("MAC verification failed")]
    MacVerificationFailed,

    // ===== Protocol Errors =====
    /// Message encoding or decoding failed.
    #[error("message error: {0}")]
    Message(String),

    /// Received an unexpected or malformed message.
    #[error("invalid message: {0}")]
    InvalidMessage(String),

    /// Peer's overlay protocol version is incompatible.
    ///
    /// The `Display` string mirrors stellar-core's on-the-wire handshake error
    /// exactly (`Peer.cpp:1851`, OVERLAY_SPEC §4.4.2-7): `"wrong protocol
    /// version"`. The wrapped `String` carries the divergence detail (below
    /// minimum / above maximum / malformed) for local diagnostics and tests —
    /// stellar-core likewise logs that detail separately (`CLOG_DEBUG`) rather
    /// than sending it to the peer.
    #[error("wrong protocol version")]
    VersionMismatch(String),

    /// Peer is on a different network (network passphrase doesn't match).
    ///
    /// Mirrors stellar-core `Peer.cpp:1869` / OVERLAY_SPEC §4.4.2-9.
    #[error("wrong network passphrase")]
    NetworkMismatch,

    // ===== Peer Management Errors =====
    /// Cannot accept more connections (limit reached).
    #[error("peer limit reached")]
    PeerLimitReached,

    /// The specified peer was not found.
    #[error("peer not found: {0}")]
    PeerNotFound(String),

    /// The peer has been banned and connections are rejected.
    #[error("peer is banned: {0}")]
    PeerBanned(String),

    /// Another handshake for the same peer ID is already in flight.
    #[error("duplicate pending peer: {0}")]
    PeerDuplicate(String),

    /// Already have an active connection to this peer.
    ///
    /// Mirrors stellar-core `Peer.cpp:1890` / OVERLAY_SPEC §4.4.2-12. The
    /// wrapped `String` is the peer ID being rejected (stellar-core appends
    /// `Config::toShortString(mPeerID)`); henyey renders the strkey peer ID.
    #[error("already-connected peer: {0}")]
    AlreadyConnected(String),

    // ===== State Errors =====
    /// Operation requires the overlay to be running.
    #[error("overlay not started")]
    NotStarted,

    /// Cannot start because overlay is already running.
    #[error("overlay already started")]
    AlreadyStarted,

    /// Operation rejected because overlay is shutting down.
    #[error("overlay is shutting down")]
    ShuttingDown,

    // ===== Address Errors =====
    /// Invalid peer address format.
    #[error("invalid peer address: {0}")]
    InvalidPeerAddress(String),

    // ===== Database Errors =====
    /// Database operation failed.
    #[error("database error: {0}")]
    DatabaseError(String),

    // ===== Wrapped Errors =====
    /// XDR serialization/deserialization error.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::curr::Error),

    /// Cryptographic operation failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] henyey_crypto::CryptoError),

    /// Low-level I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // ===== Internal Errors =====
    /// Internal channel send failed (receiver dropped).
    #[error("channel send error")]
    ChannelSend,

    /// Internal channel receive failed (sender dropped).
    #[error("channel receive error")]
    ChannelRecv,

    /// Unexpected internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl OverlayError {
    /// Returns true if this error is transient and the operation could succeed on retry.
    ///
    /// Connection failures, timeouts, and I/O errors are typically retriable.
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            OverlayError::ConnectionFailed(_)
                | OverlayError::ConnectionTimeout(_)
                | OverlayError::Io(_)
        )
    }

    /// Returns true if this error indicates a fundamental incompatibility.
    ///
    /// Network mismatches and version incompatibilities are fatal - retrying
    /// will not help and the peer should not be contacted again.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            OverlayError::NetworkMismatch | OverlayError::VersionMismatch(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Handshake error strings must mirror stellar-core `Peer.cpp` and
    // OVERLAY_SPEC §4.4.2 exactly (issue #3080). These assert the on-the-wire
    // `Display` wording, which is what gets sent to the peer in ERROR_MSG.

    #[test]
    fn test_version_mismatch_display_matches_spec() {
        // §4.4.2-7 / Peer.cpp:1851. The wrapped detail is for local diagnostics
        // only and must NOT leak into the wire string.
        let err = OverlayError::VersionMismatch("below minimum".to_string());
        assert_eq!(err.to_string(), "wrong protocol version");
    }

    #[test]
    fn test_network_mismatch_display_matches_spec() {
        // §4.4.2-9 / Peer.cpp:1869.
        let err = OverlayError::NetworkMismatch;
        assert_eq!(err.to_string(), "wrong network passphrase");
    }

    #[test]
    fn test_already_connected_display_matches_spec() {
        // §4.4.2-12 / Peer.cpp:1890 — includes the rejected peer ID.
        let err = OverlayError::AlreadyConnected(
            "GABC123EXAMPLEPEERID00000000000000000000000000000000000".to_string(),
        );
        assert_eq!(
            err.to_string(),
            "already-connected peer: GABC123EXAMPLEPEERID00000000000000000000000000000000000"
        );
    }
}
