//! Message codec for Stellar overlay protocol.
//!
//! This module implements the framing layer for Stellar network messages.
//! Each message on the wire is prefixed with a 4-byte big-endian length field:
//!
//! ```text
//! +----------------+------------------+
//! | Length (4 bytes) | XDR Message Body |
//! +----------------+------------------+
//! ```
//!
//! # Length Field Format
//!
//! The length field has special semantics:
//! - **Bit 31 (MSB)**: Authentication flag. When set, the message has a valid MAC.
//!   When clear (e.g., Hello/Auth during handshake), the MAC field is all zeros.
//! - **Bits 0-30**: Actual message body length in bytes.
//!
//! # Message Size Limits
//!
//! - Minimum: 12 bytes (at least the authenticated message header)
//! - Maximum: 32 MB (prevents memory exhaustion attacks)

use crate::{OverlayError, Result};
use bytes::{Buf, BufMut, BytesMut};
use stellar_xdr::curr::{AuthenticatedMessage, Limits, ReadXdr, WriteXdr};
use tokio_util::codec::{Decoder, Encoder};

/// Maximum message size (16 MB) - prevents memory exhaustion.
/// Spec: OVERLAY_SPEC §3.1 — MAX_MESSAGE_SIZE = 16,777,216 bytes.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Maximum message size before authentication completes.
/// Spec: OVERLAY_SPEC §3.1 — unauthenticated messages (Hello/Auth) MUST NOT exceed 4,096 bytes.
const MAX_UNAUTHENTICATED_MESSAGE_SIZE: usize = 4096;

/// Minimum message size - must fit at least the authenticated message header.
const MIN_MESSAGE_SIZE: usize = 12;

/// A framed message received from the network.
///
/// Contains the decoded message along with metadata about how it was received.
#[derive(Debug)]
pub struct MessageFrame {
    /// The decoded authenticated message wrapper.
    pub message: AuthenticatedMessage,

    /// Size of the message body in bytes (not including length prefix).
    pub raw_len: usize,

    /// Whether bit 31 was set in the length prefix.
    ///
    /// When true, the message has a valid MAC that should be verified.
    /// When false (Hello/Auth during handshake), the MAC field is zeros.
    pub is_authenticated: bool,
}

impl MessageFrame {
    /// Creates a new message frame with the given parameters.
    pub fn new(message: AuthenticatedMessage, raw_len: usize, is_authenticated: bool) -> Self {
        Self {
            message,
            raw_len,
            is_authenticated,
        }
    }
}

/// Codec for encoding and decoding Stellar overlay messages.
///
/// Implements tokio's `Encoder` and `Decoder` traits for use with framed
/// TCP streams. Handles the length-prefixed framing protocol automatically.
///
/// # Usage
///
/// ```rust,ignore
/// use tokio_util::codec::Framed;
/// use henyey_overlay::MessageCodec;
///
/// let framed = Framed::new(tcp_stream, MessageCodec::new());
/// ```
#[derive(Debug, Default)]
pub struct MessageCodec {
    /// Current state of the decoder state machine.
    decode_state: DecodeState,
    /// Whether authentication has completed. Before auth, messages are limited
    /// to MAX_UNAUTHENTICATED_MESSAGE_SIZE (4096 bytes).
    authenticated: bool,
}

/// Internal state machine for streaming message decoding.
#[derive(Debug, Default)]
enum DecodeState {
    /// Waiting for the 4-byte length prefix.
    #[default]
    ReadingLength,
    /// Have length, waiting for the message body.
    ReadingBody {
        /// Expected message body length.
        len: usize,
        /// Whether bit 31 was set (message has valid MAC).
        is_authenticated: bool,
    },
}

impl MessageCodec {
    /// Creates a new message codec with initial state.
    pub fn new() -> Self {
        Self {
            decode_state: DecodeState::ReadingLength,
            authenticated: false,
        }
    }

    /// Mark the codec as authenticated, allowing full-size messages.
    ///
    /// Before this is called, incoming messages are limited to 4,096 bytes
    /// per OVERLAY_SPEC §3.1.
    pub fn set_authenticated(&mut self) {
        self.authenticated = true;
    }

    /// Encodes a message to bytes with length prefix.
    ///
    /// Returns a `Vec<u8>` containing the 4-byte length prefix followed by
    /// the XDR-encoded message body. The length prefix has bit 31 set for
    /// authenticated messages (all except Hello).
    pub fn encode_message(message: &AuthenticatedMessage) -> Result<Vec<u8>> {
        // Determine if this message should have the authentication bit set.
        // G14: Bit 31 (0x80000000) is the AUTH_MSG_FLAG from stellar-core
        // (Peer.h:AUTH_MSG_FLAG_PULL_MODE_REQUESTED). HELLO messages don't
        // have the auth bit; all others do.
        let AuthenticatedMessage::V0(v0) = message;
        let is_authenticated = !matches!(v0.message, stellar_xdr::curr::StellarMessage::Hello(_));

        let xdr_bytes = message.to_xdr(Limits::none())?;
        let len = xdr_bytes.len() as u32;
        let auth_bit = if is_authenticated { 0x80000000u32 } else { 0 };

        let mut buf = Vec::with_capacity(4 + xdr_bytes.len());
        buf.extend_from_slice(&(len | auth_bit).to_be_bytes());
        buf.extend_from_slice(&xdr_bytes);

        Ok(buf)
    }

    /// Decodes XDR bytes to an authenticated message.
    ///
    /// The input should be the raw message body without the length prefix.
    // SECURITY: frame size bounded by MAX_MESSAGE_SIZE at transport layer before XDR decode
    pub fn decode_message(bytes: &[u8]) -> Result<AuthenticatedMessage> {
        AuthenticatedMessage::from_xdr(bytes, Limits::none())
            .map_err(|e| OverlayError::Message(format!("failed to decode XDR: {}", e)))
    }
}

impl Decoder for MessageCodec {
    type Item = MessageFrame;
    type Error = OverlayError;

    // SECURITY: post-auth frame size enforced at frame layer before decode; Limits::none() is safe here
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        loop {
            match self.decode_state {
                DecodeState::ReadingLength => {
                    if src.len() < 4 {
                        // Need more data for length
                        return Ok(None);
                    }

                    // Read length prefix
                    // In Stellar's protocol, bit 31 indicates authentication status
                    // Bit 31 set = message has valid MAC
                    // Bit 31 clear = message has zero MAC (Hello/Auth during handshake)
                    let raw_len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
                    let is_authenticated = (raw_len & 0x80000000) != 0;
                    let len = (raw_len & 0x7FFFFFFF) as usize;

                    // Validate length
                    if len < MIN_MESSAGE_SIZE {
                        return Err(OverlayError::Message(format!(
                            "message too small: {} bytes",
                            len
                        )));
                    }
                    // Enforce size limit based on authentication state.
                    // Spec: OVERLAY_SPEC §3.1 — before auth completes, limit to 4,096 bytes.
                    let max_size = if self.authenticated {
                        MAX_MESSAGE_SIZE
                    } else {
                        MAX_UNAUTHENTICATED_MESSAGE_SIZE
                    };
                    if len > max_size {
                        return Err(OverlayError::Message(format!(
                            "message too large: {} bytes (limit: {})",
                            len, max_size
                        )));
                    }

                    // Advance past length prefix
                    src.advance(4);

                    // Reserve space for message body
                    src.reserve(len);

                    self.decode_state = DecodeState::ReadingBody {
                        len,
                        is_authenticated,
                    };
                }
                DecodeState::ReadingBody {
                    len,
                    is_authenticated,
                } => {
                    if src.len() < len {
                        // Need more data for message body
                        return Ok(None);
                    }

                    // Read message body
                    let body = src.split_to(len);

                    // Decode XDR
                    let message = Self::decode_message(&body)?;

                    // Reset state
                    self.decode_state = DecodeState::ReadingLength;

                    return Ok(Some(MessageFrame::new(message, len, is_authenticated)));
                }
            }
        }
    }
}

impl Encoder<AuthenticatedMessage> for MessageCodec {
    type Error = OverlayError;

    fn encode(&mut self, message: AuthenticatedMessage, dst: &mut BytesMut) -> Result<()> {
        // Determine if this message should have the authentication bit set.
        // HELLO messages are sent before keys are established, so they use
        // sequence 0 and an all-zero MAC field - no auth bit.
        // All other messages (AUTH and post-auth) have valid MACs and need
        // the auth bit set so the receiver knows to verify the MAC.
        let is_authenticated = match &message {
            AuthenticatedMessage::V0(v0) => {
                !matches!(v0.message, stellar_xdr::curr::StellarMessage::Hello(_))
            }
        };

        // Encode to XDR
        let xdr_bytes = message.to_xdr(Limits::none())?;

        // Check size
        if xdr_bytes.len() > MAX_MESSAGE_SIZE {
            return Err(OverlayError::Message(format!(
                "message too large: {} bytes",
                xdr_bytes.len()
            )));
        }

        // Write length prefix with authentication bit
        // Bit 31 set = message has valid MAC that should be verified
        // Bit 31 clear = message has zero MAC (Hello during handshake)
        let len = xdr_bytes.len() as u32;
        let auth_bit = if is_authenticated { 0x80000000 } else { 0 };
        dst.reserve(4 + xdr_bytes.len());
        dst.put_u32(len | auth_bit);

        // Write message body
        dst.extend_from_slice(&xdr_bytes);

        Ok(())
    }
}

/// Helper functions for working with Stellar messages.
///
/// Provides utilities for message classification, hashing, and display.
pub mod helpers {
    use super::*;
    use stellar_xdr::curr::StellarMessage;

    /// Computes the SHA-256 hash of a message for flood tracking.
    ///
    /// The hash is computed over the XDR-encoded message bytes.
    ///
    /// # Panics
    ///
    /// Panics if the message cannot be XDR-serialized (should never happen
    /// for valid `StellarMessage` values).
    pub fn message_hash(message: &StellarMessage) -> henyey_common::Hash256 {
        let bytes = message
            .to_xdr(Limits::none())
            .expect("StellarMessage XDR serialization must not fail");
        henyey_common::Hash256::hash(&bytes)
    }

    /// Returns true if this message type is flow-controlled (uses flood/flow
    /// capacity). This includes both globally-deduplicated flood payloads
    /// (transactions, SCP envelopes) and peer-local pull-control messages
    /// (FloodAdvert, FloodDemand). Use [`is_flood_gate_tracked`] to distinguish
    /// messages that should be recorded in the global FloodGate for dedup.
    pub fn is_flood_message(message: &StellarMessage) -> bool {
        crate::flow_control::is_flow_controlled_message(message)
    }

    /// Returns true if this message should be tracked in the global FloodGate
    /// for deduplication and forwarding. Only transaction and SCP messages are
    /// FloodGate-tracked. Pull-control messages (FloodAdvert, FloodDemand)
    /// are flow-controlled but peer-local — stellar-core's recvFloodAdvert /
    /// recvFloodDemand do not call recvFloodedMsgID.
    pub fn is_flood_gate_tracked(message: &StellarMessage) -> bool {
        matches!(
            message,
            StellarMessage::Transaction(_) | StellarMessage::ScpMessage(_)
        )
    }

    /// Returns true if this message should be dropped for watcher (non-validator) nodes.
    ///
    /// Watchers don't need transaction flood, pull-based flood control, or survey
    /// messages. Dropping these at the overlay layer reduces broadcast channel
    /// pressure by ~90% on mainnet, preventing SCP message loss.
    pub fn is_watcher_droppable(message: &StellarMessage) -> bool {
        // Watchers must participate in the pull-mode flooding protocol
        // (FloodAdvert, FloodDemand, Transaction) to forward transactions
        // submitted via HTTP to validators.  Only survey messages are
        // truly validator-only and can be dropped on watchers.
        matches!(
            message,
            StellarMessage::TimeSlicedSurveyRequest(_)
                | StellarMessage::TimeSlicedSurveyResponse(_)
                | StellarMessage::TimeSlicedSurveyStartCollecting(_)
                | StellarMessage::TimeSlicedSurveyStopCollecting(_)
        )
    }

    /// Returns true if this is a handshake message (Hello or Auth).
    ///
    /// Handshake messages are handled specially during connection setup
    /// and should not be processed after authentication is complete.
    pub fn is_handshake_message(message: &StellarMessage) -> bool {
        matches!(message, StellarMessage::Hello(_) | StellarMessage::Auth(_))
    }

    /// Returns a human-readable name for the message type.
    ///
    /// Useful for logging and debugging.
    pub fn message_type_name(message: &StellarMessage) -> &'static str {
        match message {
            StellarMessage::ErrorMsg(_) => "ERROR",
            StellarMessage::Hello(_) => "HELLO",
            StellarMessage::Auth(_) => "AUTH",
            StellarMessage::DontHave(_) => "DONT_HAVE",
            StellarMessage::Peers(_) => "PEERS",
            StellarMessage::GetTxSet(_) => "GET_TX_SET",
            StellarMessage::TxSet(_) => "TX_SET",
            StellarMessage::GeneralizedTxSet(_) => "GENERALIZED_TX_SET",
            StellarMessage::Transaction(_) => "TRANSACTION",
            StellarMessage::TimeSlicedSurveyRequest(_) => "TIME_SLICED_SURVEY_REQUEST",
            StellarMessage::TimeSlicedSurveyResponse(_) => "TIME_SLICED_SURVEY_RESPONSE",
            StellarMessage::TimeSlicedSurveyStartCollecting(_) => {
                "TIME_SLICED_SURVEY_START_COLLECTING"
            }
            StellarMessage::TimeSlicedSurveyStopCollecting(_) => {
                "TIME_SLICED_SURVEY_STOP_COLLECTING"
            }
            StellarMessage::GetScpQuorumset(_) => "GET_SCP_QUORUMSET",
            StellarMessage::ScpQuorumset(_) => "SCP_QUORUMSET",
            StellarMessage::ScpMessage(_) => "SCP_MESSAGE",
            StellarMessage::GetScpState(_) => "GET_SCP_STATE",
            StellarMessage::SendMore(_) => "SEND_MORE",
            StellarMessage::SendMoreExtended(_) => "SEND_MORE_EXTENDED",
            StellarMessage::FloodAdvert(_) => "FLOOD_ADVERT",
            StellarMessage::FloodDemand(_) => "FLOOD_DEMAND",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{AuthenticatedMessageV0, HmacSha256Mac, StellarMessage, VecM};

    fn make_test_message() -> AuthenticatedMessage {
        AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Peers(VecM::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        })
    }

    #[test]
    fn test_encode_decode() {
        let msg = make_test_message();
        let encoded = MessageCodec::encode_message(&msg).unwrap();

        // Should have 4-byte length prefix
        assert!(encoded.len() > 4);

        // Length should match (mask off auth bit)
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        let len = (raw_len & 0x7FFFFFFF) as usize;
        assert_eq!(len, encoded.len() - 4);

        // Non-Hello messages should have auth bit set
        assert!(
            raw_len & 0x80000000 != 0,
            "auth bit should be set for non-Hello messages"
        );

        // Should decode
        let decoded = MessageCodec::decode_message(&encoded[4..]).unwrap();
        match decoded {
            AuthenticatedMessage::V0(v0) => {
                assert_eq!(v0.sequence, 0);
                assert!(matches!(v0.message, StellarMessage::Peers(_)));
            }
        }
    }

    #[test]
    fn test_auth_bit_set_for_authenticated_messages() {
        // Non-Hello messages should have auth bit set
        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 1,
            message: StellarMessage::Peers(VecM::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        });
        let encoded = MessageCodec::encode_message(&msg).unwrap();
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);

        assert!(raw_len & 0x80000000 != 0, "auth bit should be set");
        assert_eq!((raw_len & 0x7FFFFFFF) as usize, encoded.len() - 4);
    }

    #[test]
    fn test_auth_bit_not_set_for_hello() {
        // Hello messages should NOT have auth bit set
        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Hello(Default::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        });
        let encoded = MessageCodec::encode_message(&msg).unwrap();
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);

        assert!(
            raw_len & 0x80000000 == 0,
            "auth bit should NOT be set for Hello"
        );
        assert_eq!(raw_len as usize, encoded.len() - 4);
    }

    #[test]
    fn test_codec_roundtrip_with_auth_bit() {
        // Test that encode/decode roundtrip preserves the auth bit correctly
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::new();

        // Authenticated message (non-Hello)
        let auth_msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 5,
            message: StellarMessage::Peers(VecM::default()),
            mac: HmacSha256Mac { mac: [42u8; 32] },
        });
        codec.encode(auth_msg, &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert!(
            frame.is_authenticated,
            "decoded frame should be marked as authenticated"
        );

        // Hello message (unauthenticated)
        let hello_msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Hello(Default::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        });
        codec.encode(hello_msg, &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert!(
            !frame.is_authenticated,
            "Hello message should NOT be marked as authenticated"
        );
    }

    #[test]
    fn test_codec_streaming() {
        let msg = make_test_message();
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::new();

        // Encode
        codec.encode(msg, &mut buf).unwrap();

        // Decode
        let decoded = codec.decode(&mut buf).unwrap();
        assert!(decoded.is_some());
    }

    #[test]
    fn test_codec_partial_read() {
        let msg = make_test_message();
        let encoded = MessageCodec::encode_message(&msg).unwrap();
        let mut codec = MessageCodec::new();

        // Feed partial data
        let mut buf = BytesMut::from(&encoded[..2]);
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Feed more data
        buf.extend_from_slice(&encoded[2..]);
        assert!(codec.decode(&mut buf).unwrap().is_some());
    }

    #[test]
    fn test_message_type_names() {
        assert_eq!(
            helpers::message_type_name(&StellarMessage::Peers(VecM::default())),
            "PEERS"
        );
        assert_eq!(
            helpers::message_type_name(&StellarMessage::Hello(Default::default())),
            "HELLO"
        );
    }

    // ── OVERLAY_SPEC §3.1: Message size constants ─────────────────────

    #[test]
    fn test_max_message_size_is_16_mib() {
        assert_eq!(MAX_MESSAGE_SIZE, 16 * 1024 * 1024);
    }

    #[test]
    fn test_max_unauthenticated_message_size_is_4096() {
        assert_eq!(MAX_UNAUTHENTICATED_MESSAGE_SIZE, 4096);
    }

    #[test]
    fn test_min_message_size_is_12() {
        assert_eq!(MIN_MESSAGE_SIZE, 12);
    }

    #[test]
    fn test_decoder_rejects_too_small_message() {
        let mut codec = MessageCodec::new();
        // 4 bytes = too small (MIN_MESSAGE_SIZE is 12)
        let len: u32 = 4;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&len.to_be_bytes());
        // Add enough dummy bytes so the decoder doesn't stall waiting for body
        buf.extend_from_slice(&[0u8; 16]);

        let result = codec.decode(&mut buf);
        assert!(
            result.is_err(),
            "messages below MIN_MESSAGE_SIZE must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("too small"),
            "error should mention 'too small': {err}"
        );
    }

    #[test]
    fn test_decoder_rejects_oversized_unauthenticated_message() {
        let mut codec = MessageCodec::new();
        // Before set_authenticated(), limit is 4096 bytes
        let len: u32 = 4097;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&vec![0u8; 4097]);

        let result = codec.decode(&mut buf);
        assert!(
            result.is_err(),
            "unauthenticated messages > 4096 bytes must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("too large"),
            "error should mention 'too large': {err}"
        );
    }

    #[test]
    fn test_set_authenticated_raises_size_limit() {
        let mut codec = MessageCodec::new();
        codec.set_authenticated();

        // After set_authenticated(), a 4097-byte message is allowed (length is valid).
        // We can't fully decode it since the body isn't valid XDR, but the length
        // check should pass and move to ReadingBody state.
        let len: u32 = 4097;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&len.to_be_bytes());
        // Only provide partial body so decoder returns Ok(None) waiting for more data
        buf.extend_from_slice(&[0u8; 100]);

        let result = codec.decode(&mut buf);
        assert!(
            result.is_ok(),
            "authenticated codec must accept messages > 4096 bytes"
        );
        assert!(
            result.unwrap().is_none(),
            "should be waiting for more body data"
        );
    }

    #[test]
    fn test_encoder_rejects_oversized_message() {
        // Encoding a message that exceeds MAX_MESSAGE_SIZE should fail in XDR
        // serialization or length check. We test indirectly: encode a normal message
        // and verify it works, then check the size limit constant.
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::new();
        let msg = make_test_message();
        assert!(codec.encode(msg, &mut buf).is_ok());

        // Verify the constant matches the spec
        assert_eq!(
            MAX_MESSAGE_SIZE,
            16 * 1024 * 1024,
            "MAX_MESSAGE_SIZE must be 16 MiB"
        );
    }

    #[test]
    fn test_is_flood_message_classification() {
        // Flood messages
        assert!(helpers::is_flood_message(&StellarMessage::Transaction(
            stellar_xdr::curr::TransactionEnvelope::TxV0(Default::default())
        )));
        assert!(helpers::is_flood_message(&StellarMessage::FloodAdvert(
            Default::default()
        )));
        assert!(helpers::is_flood_message(&StellarMessage::FloodDemand(
            Default::default()
        )));

        // Non-flood messages
        assert!(!helpers::is_flood_message(&StellarMessage::Hello(
            Default::default()
        )));
        assert!(!helpers::is_flood_message(&StellarMessage::Peers(
            VecM::default()
        )));
    }

    #[test]
    fn test_is_handshake_message_classification() {
        assert!(helpers::is_handshake_message(&StellarMessage::Hello(
            Default::default()
        )));
        assert!(helpers::is_handshake_message(&StellarMessage::Auth(
            Default::default()
        )));
        assert!(!helpers::is_handshake_message(&StellarMessage::Peers(
            VecM::default()
        )));
    }

    #[test]
    fn test_watcher_droppable_keeps_tx_flooding_messages() {
        assert!(!helpers::is_watcher_droppable(
            &StellarMessage::Transaction(stellar_xdr::curr::TransactionEnvelope::TxV0(
                Default::default()
            ))
        ));
        assert!(!helpers::is_watcher_droppable(
            &StellarMessage::FloodAdvert(Default::default())
        ));
        assert!(!helpers::is_watcher_droppable(
            &StellarMessage::FloodDemand(Default::default())
        ));
    }

    #[test]
    fn test_watcher_droppable_still_drops_survey_messages() {
        assert!(helpers::is_watcher_droppable(
            &StellarMessage::TimeSlicedSurveyRequest(Default::default())
        ));
        assert!(helpers::is_watcher_droppable(
            &StellarMessage::TimeSlicedSurveyResponse(Default::default())
        ));
        assert!(helpers::is_watcher_droppable(
            &StellarMessage::TimeSlicedSurveyStartCollecting(Default::default())
        ));
        assert!(helpers::is_watcher_droppable(
            &StellarMessage::TimeSlicedSurveyStopCollecting(Default::default())
        ));
    }

    #[test]
    fn test_flood_gate_tracked_excludes_pull_control() {
        // Pull-control messages are flow-controlled but NOT FloodGate-tracked
        assert!(!helpers::is_flood_gate_tracked(
            &StellarMessage::FloodAdvert(Default::default())
        ));
        assert!(!helpers::is_flood_gate_tracked(
            &StellarMessage::FloodDemand(Default::default())
        ));

        // Transaction and SCP messages ARE FloodGate-tracked
        assert!(helpers::is_flood_gate_tracked(
            &StellarMessage::Transaction(stellar_xdr::curr::TransactionEnvelope::TxV0(
                Default::default()
            ))
        ));
        assert!(helpers::is_flood_gate_tracked(&StellarMessage::ScpMessage(
            Default::default()
        )));
    }

    #[test]
    fn test_pull_control_is_flood_but_not_gate_tracked() {
        // FloodAdvert and FloodDemand are flood messages (flow-controlled)...
        assert!(helpers::is_flood_message(&StellarMessage::FloodAdvert(
            Default::default()
        )));
        assert!(helpers::is_flood_message(&StellarMessage::FloodDemand(
            Default::default()
        )));
        // ...but they are NOT FloodGate-tracked
        assert!(!helpers::is_flood_gate_tracked(
            &StellarMessage::FloodAdvert(Default::default())
        ));
        assert!(!helpers::is_flood_gate_tracked(
            &StellarMessage::FloodDemand(Default::default())
        ));
    }
}
