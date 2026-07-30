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
//! The length field uses XDR record marking semantics:
//! - **Bit 31 (MSB)**: Record-marking continuation ("last fragment") flag.
//!   Henyey always sets it on send, since every overlay message is written as
//!   a single XDR record fragment. On receive the bit is **masked off and
//!   never rejected on**, matching stellar-core's
//!   `TCPPeer::getIncomingMsgLength()` (`TCPPeer.cpp:673-686`), which does
//!   `length &= 0x7f` and never inspects the bit. It is surfaced as
//!   [`MessageFrame::is_last_fragment`] for diagnostics only.
//! - **Bits 0-30**: Actual message body length in bytes.
//!
//! # Message Size Limits
//!
//! - Minimum: 12 bytes (at least the authenticated message header)
//! - Maximum: 16 MB (prevents memory exhaustion attacks), matching
//!   stellar-core's `Peer.h` `MAX_MESSAGE_SIZE = 1024 * 1024 * 16`.
//! - Pre-authentication maximum: 4,096 bytes (applies to HELLO/AUTH before the
//!   handshake completes), matching stellar-core's `TCPPeer.h`
//!   `MAX_UNAUTH_MESSAGE_SIZE = 0x1000`.

use crate::{OverlayError, Result};
use bytes::{Buf, BufMut, BytesMut};
use stellar_xdr::{AuthenticatedMessage, Limits, ReadXdr, WriteXdr};
use tokio_util::codec::{Decoder, Encoder};

/// Maximum message size (16 MB) - prevents memory exhaustion.
/// Spec: OVERLAY_SPEC §3.3 — MAX_MESSAGE_SIZE = 16,777,216 bytes.
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Maximum message size before authentication completes.
/// Spec: OVERLAY_SPEC §3.3 — unauthenticated messages (Hello/Auth) MUST NOT exceed 4,096 bytes.
const MAX_UNAUTHENTICATED_MESSAGE_SIZE: usize = 4096;

/// Minimum message size - must fit at least the authenticated message header.
const MIN_MESSAGE_SIZE: usize = 12;

/// Rejects an outbound XDR payload whose length exceeds `MAX_MESSAGE_SIZE`.
///
/// This is the single enforcement point for the encode-side size bound, shared
/// by the real send-path encoder (`MessageCodec::encode_message`) and the
/// `Encoder` trait impl. Keeping one implementation prevents the two paths from
/// silently diverging again (see #3774). Mirrors the receive-side rejection in
/// `Decoder::decode` and stellar-core's `TCPPeer.cpp:690-701`.
fn check_encode_size(xdr_len: usize) -> Result<()> {
    if xdr_len > MAX_MESSAGE_SIZE {
        return Err(OverlayError::Message(format!(
            "message too large: {} bytes",
            xdr_len
        )));
    }
    Ok(())
}

/// A framed message received from the network.
///
/// Contains the decoded message along with metadata about how it was received.
#[derive(Debug)]
pub struct MessageFrame {
    /// The decoded authenticated message wrapper.
    pub message: AuthenticatedMessage,

    /// Size of the message body in bytes (not including length prefix).
    pub raw_len: usize,

    /// Whether bit 31 of the 4-byte length prefix was set.
    ///
    /// Invariant: bit 31 is the XDR record-marking "last fragment"
    /// (continuation) bit, NOT an authentication flag. stellar-core treats
    /// it identically (`TCPPeer.cpp:679`: `length &= 0x7f` clears the XDR
    /// continuation bit). MAC/auth gating is on the receiver's own auth
    /// state (see `AuthContext::unwrap_message`), never on this wire bit.
    pub is_last_fragment: bool,
}

impl MessageFrame {
    /// Creates a new message frame with the given parameters.
    pub fn new(message: AuthenticatedMessage, raw_len: usize, is_last_fragment: bool) -> Self {
        Self {
            message,
            raw_len,
            is_last_fragment,
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
        /// Whether bit 31 was set (final XDR record fragment).
        is_last_fragment: bool,
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
    /// per OVERLAY_SPEC §3.3.
    pub fn set_authenticated(&mut self) {
        self.authenticated = true;
    }

    /// Encodes a message to bytes with length prefix.
    ///
    /// Returns a `Vec<u8>` containing the 4-byte record-marking prefix followed
    /// by the XDR-encoded message body. Bit 31 is always set because Henyey
    /// writes each overlay message as a single XDR record fragment.
    pub fn encode_message(message: &AuthenticatedMessage) -> Result<Vec<u8>> {
        let xdr_bytes = message.to_xdr(Limits::none())?;
        check_encode_size(xdr_bytes.len())?;
        let len = xdr_bytes.len() as u32;

        let mut buf = Vec::with_capacity(4 + xdr_bytes.len());
        buf.extend_from_slice(&(len | 0x80000000u32).to_be_bytes());
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

                    // Read XDR record-marking prefix. Bit 31 is the
                    // continuation ("last fragment") flag, not an
                    // authentication marker. It is masked off and never
                    // rejected on: stellar-core's
                    // `TCPPeer::getIncomingMsgLength()` (TCPPeer.cpp:673-686)
                    // does `length &= 0x7f` and never inspects the bit, so
                    // rejecting a clear bit would drop peers stellar-core
                    // keeps (#3776). The value is retained purely as
                    // descriptive metadata on `MessageFrame`.
                    let raw_len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
                    let is_last_fragment = (raw_len & 0x80000000) != 0;
                    let len = (raw_len & 0x7FFFFFFF) as usize;

                    // Validate length
                    // OVERLAY_SPEC §3.3: len==0 is handled distinctly as
                    // "error during read", matching stellar-core
                    // TCPPeer.cpp:690-700.
                    if len == 0 {
                        return Err(OverlayError::Message(
                            "error during read: zero-length message".into(),
                        ));
                    }
                    if len < MIN_MESSAGE_SIZE {
                        return Err(OverlayError::Message(format!(
                            "message too small: {} bytes",
                            len
                        )));
                    }
                    // Enforce size limit based on authentication state.
                    // Spec: OVERLAY_SPEC §3.3 — before auth completes, limit to 4,096 bytes.
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
                        is_last_fragment,
                    };
                }
                DecodeState::ReadingBody {
                    len,
                    is_last_fragment,
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

                    return Ok(Some(MessageFrame::new(message, len, is_last_fragment)));
                }
            }
        }
    }
}

impl Encoder<AuthenticatedMessage> for MessageCodec {
    type Error = OverlayError;

    fn encode(&mut self, message: AuthenticatedMessage, dst: &mut BytesMut) -> Result<()> {
        // Encode to XDR
        let xdr_bytes = message.to_xdr(Limits::none())?;

        // Check size (shared with the real send-path encoder, `encode_message`).
        check_encode_size(xdr_bytes.len())?;

        // Write XDR record-marking prefix. Bit 31 is the final-fragment bit,
        // not an authentication marker.
        let len = xdr_bytes.len() as u32;
        dst.reserve(4 + xdr_bytes.len());
        dst.put_u32(len | 0x80000000);

        // Write message body
        dst.extend_from_slice(&xdr_bytes);

        Ok(())
    }
}

/// Helper functions for working with Stellar messages.
///
/// Provides utilities for message classification and display.
///
/// # Flood deduplication hashing
///
/// This module does **not** provide the flood-dedup hash. Per
/// `OVERLAY_SPEC.md` §3.3, flood message deduplication uses **BLAKE2b-256**
/// over the XDR serialization of the entire `StellarMessage` (matching
/// stellar-core's `xdrBlake2()` in `Floodgate::broadcast()`) — **not**
/// SHA-256. The authoritative implementation is
/// [`crate::flood::compute_message_hash`]; all flood-gate call sites use it.
pub mod helpers {
    use stellar_xdr::StellarMessage;

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

    /// Returns true for message types that should be dropped when the node is not synced.
    ///
    /// Parity: Peer.cpp:1164-1166 — `ignoreIfOutOfSync` covers TRANSACTION,
    /// FLOOD_ADVERT, FLOOD_DEMAND. During catchup, transactions are rejected by
    /// the herder anyway and flood-pull responses reference messages the node
    /// can't use. Dropping early avoids flood-gate, rate-limiter, clone, and
    /// channel work.
    pub fn is_flood_shed_on_unsync(message: &StellarMessage) -> bool {
        matches!(
            message,
            StellarMessage::Transaction(_)
                | StellarMessage::FloodAdvert(_)
                | StellarMessage::FloodDemand(_)
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
    /// Delegates to [`OverlayMessageKind::wire_name`] — the single source of
    /// truth for message classification.
    pub fn message_type_name(message: &StellarMessage) -> &'static str {
        crate::metrics::OverlayMessageKind::from_stellar_message(message).wire_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        AuthenticatedMessageV0, BytesM, HmacSha256Mac, NodeId, PublicKey, ScpBallot, ScpEnvelope,
        ScpStatement, ScpStatementPledges, ScpStatementPrepare, Signature, StellarMessage, Uint256,
        Value, VecM,
    };

    fn make_test_message() -> AuthenticatedMessage {
        AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Peers(VecM::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        })
    }

    /// Constructs a real, well-formed `AuthenticatedMessage` whose XDR
    /// serialization exceeds `MAX_MESSAGE_SIZE`. The oversized bytes live in
    /// the unbounded `ScpBallot.value` opaque blob (`Value(BytesM)`), reachable
    /// via `StellarMessage::ScpMessage`. This is a genuine send-path message,
    /// not a synthetic byte buffer — it exercises `encode_message` end to end.
    fn make_oversized_message() -> AuthenticatedMessage {
        let oversized_value = Value(
            BytesM::try_from(vec![0u8; MAX_MESSAGE_SIZE + 1])
                .expect("Value is an unbounded opaque blob"),
        );
        let statement = ScpStatement {
            node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            slot_index: 0,
            pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                quorum_set_hash: stellar_xdr::Hash([0u8; 32]),
                ballot: ScpBallot {
                    counter: 0,
                    value: oversized_value,
                },
                prepared: None,
                prepared_prime: None,
                n_c: 0,
                n_h: 0,
            }),
        };
        AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::ScpMessage(ScpEnvelope {
                statement,
                signature: Signature(BytesM::default()),
            }),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        })
    }

    #[test]
    fn test_encode_decode() {
        let msg = make_test_message();
        let encoded = MessageCodec::encode_message(&msg).unwrap();

        // Should have 4-byte length prefix
        assert!(encoded.len() > 4);

        // Length should match (mask off final-fragment bit)
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        let len = (raw_len & 0x7FFFFFFF) as usize;
        assert_eq!(len, encoded.len() - 4);

        // Single-fragment messages should have final-fragment bit set
        assert!(
            raw_len & 0x80000000 != 0,
            "final-fragment bit should be set"
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
    fn test_record_marking_bit_set_for_authenticated_messages() {
        // All single-fragment messages should have the XDR final-fragment bit set.
        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 1,
            message: StellarMessage::Peers(VecM::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        });
        let encoded = MessageCodec::encode_message(&msg).unwrap();
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);

        assert!(
            raw_len & 0x80000000 != 0,
            "final-fragment bit should be set"
        );
        assert_eq!((raw_len & 0x7FFFFFFF) as usize, encoded.len() - 4);
    }

    #[test]
    fn test_record_marking_bit_set_for_hello() {
        // HELLO is also a complete single-fragment XDR record.
        let msg = AuthenticatedMessage::V0(AuthenticatedMessageV0 {
            sequence: 0,
            message: StellarMessage::Hello(Default::default()),
            mac: HmacSha256Mac { mac: [0u8; 32] },
        });
        let encoded = MessageCodec::encode_message(&msg).unwrap();
        let raw_len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);

        assert!(
            raw_len & 0x80000000 != 0,
            "final-fragment bit should be set for Hello"
        );
        assert_eq!((raw_len & 0x7FFFFFFF) as usize, encoded.len() - 4);
    }

    #[test]
    fn test_codec_roundtrip_with_record_marking_bit() {
        // Test that encode/decode roundtrip preserves the final-fragment bit.
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
            frame.is_last_fragment,
            "decoded frame should be marked as final fragment"
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
            frame.is_last_fragment,
            "Hello message should be marked as final fragment"
        );
    }

    #[test]
    fn test_decode_accepts_continuation_bit_clear() {
        // Regression for #3776: a frame whose XDR record-marking bit 31 is
        // CLEAR must decode exactly like the bit-31-set equivalent.
        // stellar-core's `TCPPeer::getIncomingMsgLength()` masks the bit off
        // (`length &= 0x7f`) and never inspects it, and stellar-core's own
        // writer (xdrpp `marshal.cc`) always sets it and never implements
        // continuation fragments — so rejecting a clear bit is strictly
        // stricter than upstream and drops peers stellar-core would keep.
        let msg = make_test_message();

        // Reference frame: bit 31 set (what `encode_message` emits).
        let with_bit = MessageCodec::encode_message(&msg).unwrap();
        let body = &with_bit[4..];

        // Same frame, bit 31 cleared in the length prefix.
        let mut without_bit = Vec::with_capacity(with_bit.len());
        without_bit.extend_from_slice(&(body.len() as u32).to_be_bytes());
        without_bit.extend_from_slice(body);

        // Sanity: the two differ only in bit 31 of the prefix.
        let raw_len_set = u32::from_be_bytes([with_bit[0], with_bit[1], with_bit[2], with_bit[3]]);
        let raw_len_clear = u32::from_be_bytes([
            without_bit[0],
            without_bit[1],
            without_bit[2],
            without_bit[3],
        ]);
        assert_eq!(raw_len_set & 0x80000000, 0x80000000);
        assert_eq!(raw_len_clear & 0x80000000, 0);
        assert_eq!(raw_len_set & 0x7FFFFFFF, raw_len_clear & 0x7FFFFFFF);

        // Decode the bit-31-set frame for comparison.
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::from(&with_bit[..]);
        let expected = codec
            .decode(&mut buf)
            .expect("bit-31-set frame should decode")
            .expect("bit-31-set frame should yield a complete frame");
        assert!(expected.is_last_fragment);

        // Decode the bit-31-clear frame: must succeed identically, only with
        // `is_last_fragment == false` as descriptive metadata.
        let mut codec = MessageCodec::new();
        let mut buf = BytesMut::from(&without_bit[..]);
        let frame = codec
            .decode(&mut buf)
            .expect("bit-31-clear frame must not be rejected (#3776)")
            .expect("bit-31-clear frame should yield a complete frame");

        assert!(
            !frame.is_last_fragment,
            "bit 31 clear should be reported as is_last_fragment == false"
        );
        assert_eq!(frame.raw_len, expected.raw_len);
        match (frame.message, expected.message) {
            (AuthenticatedMessage::V0(got), AuthenticatedMessage::V0(want)) => {
                assert_eq!(got.sequence, want.sequence);
                assert_eq!(got.mac.mac, want.mac.mac);
                assert!(matches!(got.message, StellarMessage::Peers(_)));
            }
        }
        assert!(buf.is_empty(), "the whole frame should have been consumed");
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

    // ── OVERLAY_SPEC §3.3: Message size constants ─────────────────────

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
        let len: u32 = 4 | 0x80000000;
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
    fn test_decoder_rejects_zero_length_message() {
        // OVERLAY_SPEC §3.3: zero-length messages produce a distinct
        // "error during read" error, matching stellar-core TCPPeer.cpp:690-700.
        let mut codec = MessageCodec::new();
        let len: u32 = 0x80000000;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&[0u8; 16]);

        let result = codec.decode(&mut buf);
        assert!(result.is_err(), "zero-length message must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("error during read"),
            "error should mention 'error during read': {err}"
        );
    }

    #[test]
    fn test_decoder_rejects_oversized_unauthenticated_message() {
        let mut codec = MessageCodec::new();
        // Before set_authenticated(), limit is 4096 bytes
        let len: u32 = 4097 | 0x80000000;
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
        let len: u32 = 4097 | 0x80000000;
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
    fn test_check_encode_size_rejects_oversized() {
        // Exactly at the bound is allowed; one byte over is rejected.
        assert!(check_encode_size(MAX_MESSAGE_SIZE).is_ok());
        assert!(check_encode_size(MAX_MESSAGE_SIZE + 1).is_err());
        assert!(check_encode_size(0).is_ok());
    }

    #[test]
    fn test_encode_message_rejects_oversized_message() {
        // Regression for #3774: the real send-path encoder `encode_message`
        // (called by Connection::send / Peer::send_batch) must reject a frame
        // whose XDR body exceeds MAX_MESSAGE_SIZE, matching stellar-core's
        // receive-side rejection (TCPPeer.cpp:690-701). Before the fix,
        // encode_message had no size check and returned Ok for this message.
        let oversized = make_oversized_message();
        let result = MessageCodec::encode_message(&oversized);
        assert!(
            result.is_err(),
            "encode_message must reject a message larger than MAX_MESSAGE_SIZE"
        );

        // A normally-sized message still encodes successfully.
        assert!(MessageCodec::encode_message(&make_test_message()).is_ok());

        // Fold in the spec-constant assertion from the old weak test.
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
            stellar_xdr::TransactionEnvelope::TxV0(Default::default())
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
            &StellarMessage::Transaction(
                stellar_xdr::TransactionEnvelope::TxV0(Default::default())
            )
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
            &StellarMessage::Transaction(
                stellar_xdr::TransactionEnvelope::TxV0(Default::default())
            )
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

    /// Parity: Peer.cpp:1164-1166 — Transaction, FloodAdvert, FloodDemand are
    /// shed when the node is not synced. All other message types pass through.
    #[test]
    fn test_is_flood_shed_on_unsync() {
        // These three message types should be shed when not synced.
        assert!(helpers::is_flood_shed_on_unsync(
            &StellarMessage::Transaction(Default::default())
        ));
        assert!(helpers::is_flood_shed_on_unsync(
            &StellarMessage::FloodAdvert(Default::default())
        ));
        assert!(helpers::is_flood_shed_on_unsync(
            &StellarMessage::FloodDemand(Default::default())
        ));

        // All other message types should NOT be shed.
        assert!(!helpers::is_flood_shed_on_unsync(&StellarMessage::Peers(
            Default::default()
        )));
        assert!(!helpers::is_flood_shed_on_unsync(
            &StellarMessage::ErrorMsg(Default::default())
        ));
        assert!(!helpers::is_flood_shed_on_unsync(
            &StellarMessage::GetScpState(0)
        ));
        assert!(!helpers::is_flood_shed_on_unsync(
            &StellarMessage::SendMore(Default::default())
        ));
        assert!(!helpers::is_flood_shed_on_unsync(
            &StellarMessage::SendMoreExtended(Default::default())
        ));
    }
}
