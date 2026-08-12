//! Peer connection handling for Stellar overlay.
//!
//! This module provides the [`Peer`] type which represents a fully authenticated
//! connection to another Stellar node. A peer encapsulates:
//!
//! - The underlying TCP connection with message framing
//! - Authentication state and MAC keys for message verification
//! - Connection metadata (peer ID, address, versions)
//! - Statistics tracking (messages sent/received, bytes transferred)
//!
//! # Lifecycle
//!
//! 1. **Connection**: Either [`Peer::connect`] (outbound) or [`Peer::accept`] (inbound)
//! 2. **Handshake**: Hello/Auth message exchange establishes authenticated channel
//! 3. **Message Exchange**: Use [`send`](Peer::send) and [`recv`](Peer::recv) for communication
//! 4. **Disconnection**: Call [`close`](Peer::close) or let the peer drop
//!
//! # Flow Control
//!
//! Peers implement Stellar's flow control protocol. After receiving messages,
//! you should call [`send_more_extended`](Peer::send_more_extended) to indicate
//! capacity for more messages.

use crate::{
    auth::AuthContext,
    codec::helpers,
    connection::{Connection, ConnectionDirection},
    flow_control::msg_body_size,
    manager::{sanitize_error_msg, PendingPeerEntry},
    metrics::{OverlayMessageKind, OverlayMetrics},
    LocalNode, OverlayError, PeerAddress, PeerId, Result,
};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use stellar_xdr::{Auth, Hello, StellarMessage};
use tracing::{debug, info, trace, warn};

/// Auth flag value indicating flow control with byte-level capacity is enabled.
///
/// Defined in the XDR spec as `AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED = 200`.
/// Both peers must set this flag in their Auth message to enable byte-based
/// flow control (as opposed to the legacy message-only mode).
const AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED: i32 = 200;

/// Current state of a peer connection.
///
/// Tracks the connection lifecycle from initial connection through
/// authentication to disconnection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// TCP connection in progress (outbound only).
    Connecting,
    /// TCP connected, Hello/Auth handshake in progress.
    Handshaking,
    /// Handshake complete, peer is ready for message exchange.
    Authenticated,
    /// Connection is being closed.
    Closing,
    /// Connection has been closed.
    Disconnected,
}

impl PeerState {
    /// Returns true if the TCP connection is established.
    ///
    /// This includes both handshaking and authenticated states.
    pub fn is_connected(&self) -> bool {
        matches!(self, PeerState::Handshaking | PeerState::Authenticated)
    }

    /// Returns true if the peer is fully authenticated and ready for messages.
    pub fn is_ready(&self) -> bool {
        matches!(self, PeerState::Authenticated)
    }
}

/// Thread-safe statistics counters for a peer connection.
///
/// All counters use relaxed atomic ordering since exact accuracy
/// is not critical for statistics.
#[derive(Debug, Default)]
pub struct PeerStats {
    /// Total number of messages sent to this peer.
    pub messages_sent: AtomicU64,
    /// Total number of messages received from this peer.
    pub messages_received: AtomicU64,
    /// Total bytes sent to this peer.
    pub bytes_sent: AtomicU64,
    /// Total bytes received from this peer.
    pub bytes_received: AtomicU64,
    /// Unique flood messages received (first time seeing message).
    pub unique_flood_messages_recv: AtomicU64,
    /// Duplicate flood messages received (already seen via another peer).
    pub duplicate_flood_messages_recv: AtomicU64,
    /// Bytes from unique flood messages.
    pub unique_flood_bytes_recv: AtomicU64,
    /// Bytes from duplicate flood messages.
    pub duplicate_flood_bytes_recv: AtomicU64,
    /// Unique fetch response messages received.
    pub unique_fetch_messages_recv: AtomicU64,
    /// Duplicate fetch response messages received.
    pub duplicate_fetch_messages_recv: AtomicU64,
    /// Bytes from unique fetch responses.
    pub unique_fetch_bytes_recv: AtomicU64,
    /// Bytes from duplicate fetch responses.
    pub duplicate_fetch_bytes_recv: AtomicU64,
}

impl PeerStats {
    /// Creates a point-in-time snapshot of all counters.
    ///
    /// The snapshot values may not be perfectly consistent with each other
    /// since each counter is read independently.
    pub fn snapshot(&self) -> PeerStatsSnapshot {
        PeerStatsSnapshot {
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            unique_flood_messages_recv: self.unique_flood_messages_recv.load(Ordering::Relaxed),
            duplicate_flood_messages_recv: self
                .duplicate_flood_messages_recv
                .load(Ordering::Relaxed),
            unique_flood_bytes_recv: self.unique_flood_bytes_recv.load(Ordering::Relaxed),
            duplicate_flood_bytes_recv: self.duplicate_flood_bytes_recv.load(Ordering::Relaxed),
            unique_fetch_messages_recv: self.unique_fetch_messages_recv.load(Ordering::Relaxed),
            duplicate_fetch_messages_recv: self
                .duplicate_fetch_messages_recv
                .load(Ordering::Relaxed),
            unique_fetch_bytes_recv: self.unique_fetch_bytes_recv.load(Ordering::Relaxed),
            duplicate_fetch_bytes_recv: self.duplicate_fetch_bytes_recv.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of peer statistics.
///
/// All values are captured atomically but may not be perfectly consistent
/// with each other (one counter might be slightly more up-to-date than another).
#[derive(Debug, Clone, Default)]
pub struct PeerStatsSnapshot {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub unique_flood_messages_recv: u64,
    pub duplicate_flood_messages_recv: u64,
    pub unique_flood_bytes_recv: u64,
    pub duplicate_flood_bytes_recv: u64,
    pub unique_fetch_messages_recv: u64,
    pub duplicate_fetch_messages_recv: u64,
    pub unique_fetch_bytes_recv: u64,
    pub duplicate_fetch_bytes_recv: u64,
}

/// Static information about a connected peer.
///
/// This information is established during the Hello handshake and
/// does not change for the lifetime of the connection.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// The peer's unique identifier (their public key).
    pub peer_id: PeerId,
    /// The peer's network address (IP and port).
    pub address: SocketAddr,
    /// Whether we initiated this connection or they did.
    pub direction: ConnectionDirection,
    /// The peer's software version string (e.g., "stellar-core v21.0.0").
    pub version_string: String,
    /// The peer's overlay protocol version.
    pub overlay_version: u32,
    /// The peer's ledger protocol version.
    pub ledger_version: u32,
    /// When this connection was established.
    pub connected_at: Instant,
    /// Original address used to connect (for outbound connections).
    /// This preserves the hostname if connecting by hostname.
    pub original_address: Option<PeerAddress>,
}

/// A fully authenticated connection to a Stellar peer.
///
/// Handles message sending and receiving with automatic MAC authentication.
/// Use [`Peer::connect`] for outbound connections or [`Peer::accept`] for inbound.
///
/// # Thread Safety
///
/// `Peer` is not `Sync` and should be accessed from a single task. For concurrent
/// access, wrap it in a `Mutex` or use the [`OverlayManager`] which handles this.
///
/// [`OverlayManager`]: crate::OverlayManager
/// Capacity of the per-peer outbound diagnostic ring (#3773). Sized at ~1.6x
/// the largest atomic flow-control batch
/// ([`FlowControlConfig::flow_control_send_more_batch_size`], default 40) so a
/// full flood batch plus its handshake/control preamble fits inside the
/// retained window. Purely a diagnostic bound — not an observable-surface
/// value.
///
/// [`FlowControlConfig::flow_control_send_more_batch_size`]: crate::flow_control::FlowControlConfig::flow_control_send_more_batch_size
const RECENT_SENDS_CAPACITY: usize = 64;

/// One entry in the per-peer outbound diagnostic ring (#3773): a record of a
/// single frame henyey sent, retained so that when a peer reports
/// `ERR_DATA "received corrupt XDR"` the frames we emitted just before can be
/// dumped alongside the warning. Diagnostic-only — never gates any logic and
/// never touches the wire.
#[derive(Debug, Clone)]
struct RecentSend {
    /// When the frame was sent (for age-at-drop in the summary).
    at: Instant,
    /// Wire name of the `StellarMessage` (e.g. `GENERALIZED_TX_SET`).
    msg_type: &'static str,
    /// On-the-wire body size in bytes (excludes the 4-byte length prefix).
    wire_size: u64,
    /// First bytes of the encoded XDR body, for byte-pattern triage.
    prefix: [u8; crate::connection::SEND_PREFIX_LEN],
}

pub struct Peer {
    /// Peer info.
    info: PeerInfo,
    /// Current state.
    state: PeerState,
    /// TCP connection.
    connection: Connection,
    /// Authentication context.
    auth: AuthContext,
    /// Statistics.
    stats: Arc<PeerStats>,
    /// Shared overlay-wide metrics. The same `Arc` is held by `SharedPeerState`,
    /// so per-peer increments aggregate into the overlay totals exposed via
    /// `/metrics`.
    metrics: Arc<OverlayMetrics>,
    /// Whether this peer currently owns a pending_peer_id reservation.
    /// Used to conditionally release the reservation on cleanup; inbound
    /// peers that bypassed reservation (mutual-dial) must not release it.
    holds_pending_peer_id: bool,
    /// Diagnostic (#3419, observability-only): the wire name of the most-recent
    /// `StellarMessage` we sent on this connection (handshake or post-auth).
    /// Updated on every send path; read by the peer loop on drop to surface the
    /// "last message henyey sent before the connection reset" pattern. `"none"`
    /// until the first send. Purely additive state — does NOT gate any logic.
    last_sent_msg_type: &'static str,
    /// Diagnostic (#3773, observability-only): a bounded ring of the most-recent
    /// outbound frames on this connection — type, wire size, timestamp, and a
    /// byte prefix of the actual encoded body. Dumped by the peer loop when a
    /// peer reports `ERR_DATA "received corrupt XDR"`, to identify which frame
    /// henyey sent that the peer could not decode. Capped at
    /// [`RECENT_SENDS_CAPACITY`]. Purely additive state — does NOT gate any
    /// logic and never alters the wire.
    recent_sends: VecDeque<RecentSend>,
}

impl Peer {
    /// Connect to a peer and perform handshake.
    /// Create an outbound peer from a pre-established transport connection.
    ///
    /// `initial_byte_grant` is the byte capacity sent in the initial
    /// SEND_MORE_EXTENDED — typically from [`FlowControlBytesConfig::bytes_total`].
    /// `initial_message_grant` is the message-level flood reading capacity
    /// (OVERLAY_SPEC §5.4.4, stellar-core `PEER_FLOOD_READING_CAPACITY`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect_with_connection(
        addr: &PeerAddress,
        connection: Connection,
        local_node: LocalNode,
        auth_timeout_secs: u64,
        pending_peer_ids: Option<Arc<DashMap<PeerId, PendingPeerEntry>>>,
        initial_byte_grant: u32,
        initial_message_grant: u32,
        metrics: Arc<OverlayMetrics>,
    ) -> Result<Self> {
        let auth = AuthContext::new(local_node, true);

        let mut peer = Self {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: connection.remote_addr(),
                direction: ConnectionDirection::Outbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: Some(addr.clone()),
            },
            state: PeerState::Connecting,
            connection,
            auth,
            stats: Arc::new(PeerStats::default()),
            metrics,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };

        peer.handshake(
            auth_timeout_secs,
            None,
            pending_peer_ids,
            initial_byte_grant,
            initial_message_grant,
        )
        .await?;
        Ok(peer)
    }

    /// Create a peer from an accepted connection.
    ///
    /// `initial_byte_grant` is the byte capacity sent in the initial
    /// SEND_MORE_EXTENDED — typically from [`FlowControlBytesConfig::bytes_total`].
    /// `initial_message_grant` is the message-level flood reading capacity
    /// (OVERLAY_SPEC §5.4.4, stellar-core `PEER_FLOOD_READING_CAPACITY`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn accept(
        connection: Connection,
        local_node: LocalNode,
        timeout_secs: u64,
        banned_peers: Arc<RwLock<HashSet<PeerId>>>,
        pending_peer_ids: Arc<DashMap<PeerId, PendingPeerEntry>>,
        initial_byte_grant: u32,
        initial_message_grant: u32,
        metrics: Arc<OverlayMetrics>,
    ) -> Result<Self> {
        debug!("Accepting peer from: {}", connection.remote_addr());

        // Create auth context (they called us)
        let auth = AuthContext::new(local_node, false);

        let mut peer = Self {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: connection.remote_addr(),
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Connecting,
            connection,
            auth,
            stats: Arc::new(PeerStats::default()),
            metrics,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };

        // Perform handshake (with ban + pending-dedup checks after HELLO for inbound)
        peer.handshake(
            timeout_secs,
            Some(banned_peers),
            Some(pending_peer_ids),
            initial_byte_grant,
            initial_message_grant,
        )
        .await?;

        Ok(peer)
    }

    /// Perform the authenticated handshake with a peer.
    ///
    /// OVERLAY_SPEC §5.4: The handshake ordering depends on direction:
    ///
    /// **Initiator (outbound)**: Send HELLO -> Receive HELLO -> Send AUTH -> Receive AUTH
    /// **Responder (inbound)**:  Receive HELLO -> Send HELLO -> Receive AUTH -> Send AUTH
    ///
    /// After authentication, both sides exchange SEND_MORE_EXTENDED for flow
    /// control and GET_SCP_STATE to synchronize consensus state.
    async fn handshake(
        &mut self,
        auth_timeout_secs: u64,
        banned_peers: Option<Arc<RwLock<HashSet<PeerId>>>>,
        pending_peer_ids: Option<Arc<DashMap<PeerId, PendingPeerEntry>>>,
        initial_byte_grant: u32,
        initial_message_grant: u32,
    ) -> Result<()> {
        self.state = PeerState::Handshaking;
        let handshake_start = std::time::Instant::now();
        debug!("Starting handshake with {}", self.connection.remote_addr());

        if self.connection.we_called_remote() {
            // --- Initiator (outbound): Send HELLO first, then receive ---
            // The initiator already sent its HELLO, so it runs both phases of
            // HELLO processing back-to-back: recv_hello (phase-1) then phase-2
            // (version → self → network → port). On a phase-2 failure it sends
            // ERR_CONF then drops — observably identical to the prior behavior.
            self.send_hello().await?;
            let peer_hello = self.recv_hello(auth_timeout_secs).await?;
            if let Err(e) = self.validate_hello_phase2(&peer_hello) {
                self.try_send_err_conf(&e).await;
                return Err(e);
            }

            // Reserve pending peer_id after learning remote identity.
            // Matches stellar-core Peer::recvHello() duplicate check.
            if let Some(ref pending) = pending_peer_ids {
                use dashmap::mapref::entry::Entry;
                match pending.entry(self.info.peer_id.clone()) {
                    Entry::Occupied(_) => {
                        warn!(
                            "Rejected duplicate outbound peer {} — handshake already in flight",
                            self.info.peer_id
                        );
                        return Err(OverlayError::PeerDuplicate(self.info.peer_id.to_string()));
                    }
                    Entry::Vacant(e) => {
                        e.insert(PendingPeerEntry {
                            reserved_at: Instant::now(),
                            direction: ConnectionDirection::Outbound,
                        });
                        self.holds_pending_peer_id = true;
                    }
                }
            }

            let result: Result<()> = async {
                self.send_auth_msg().await?;
                self.recv_auth(auth_timeout_secs).await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                if self.holds_pending_peer_id {
                    if let Some(ref pending) = pending_peer_ids {
                        pending.remove(&self.info.peer_id);
                    }
                }
                return Err(e);
            }
        } else {
            // --- Responder (inbound): Receive HELLO first, then reply ---
            // OVERLAY §4.4.2-6: HELLO must be echoed BEFORE the
            // overlay-version / self-connection / network-ID / port checks, so
            // the remote (still awaiting an unauthenticated HELLO) can decode
            // the subsequent seq-0 / zero-MAC ERROR_MSG. recv_hello runs only
            // phase-1 here (cert + keys + state); phase-2 runs after send_hello.
            let peer_hello = self.recv_hello(auth_timeout_secs).await?;

            // Check ban status immediately after learning peer identity,
            // before sending any response. Mirrors stellar-core's
            // Peer::recvHello() which checks isBanned() before AUTH.
            if let Some(ref banned) = banned_peers {
                if banned.read().contains(&self.info.peer_id) {
                    warn!(
                        "Rejected banned inbound peer {} during handshake",
                        self.info.peer_id
                    );
                    return Err(OverlayError::PeerBanned(self.info.peer_id.to_string()));
                }
            }

            // Direction-aware pending peer-ID reservation.
            //
            // If the existing reservation is from an OUTBOUND handshake, this
            // is a mutual-dial scenario: both sides dialed simultaneously.
            // We allow the inbound to proceed — the final `register_peer`
            // DashMap::entry ensures only one peer object is registered.
            //
            // If the existing reservation is from another INBOUND, this is a
            // true duplicate (e.g. the remote opened two TCP connections) and
            // we reject immediately to prevent resource waste.
            if let Some(ref pending) = pending_peer_ids {
                use dashmap::mapref::entry::Entry;
                match pending.entry(self.info.peer_id.clone()) {
                    Entry::Occupied(existing) => {
                        if existing.get().direction == ConnectionDirection::Inbound {
                            warn!(
                                "Rejected duplicate inbound peer {} — inbound handshake already in flight",
                                self.info.peer_id
                            );
                            return Err(OverlayError::PeerDuplicate(self.info.peer_id.to_string()));
                        }
                        // Outbound reservation exists → mutual-dial; proceed
                        // without taking ownership of the reservation.
                        debug!(
                            "Mutual-dial detected for peer {} — inbound bypassing pending reservation",
                            self.info.peer_id
                        );
                    }
                    Entry::Vacant(e) => {
                        e.insert(PendingPeerEntry {
                            reserved_at: Instant::now(),
                            direction: ConnectionDirection::Inbound,
                        });
                        self.holds_pending_peer_id = true;
                    }
                }
            }

            // Remaining handshake steps after peer_id reservation.
            // If any step fails, clean up the pending peer_id reservation
            // only if we own it.
            //
            // Ordering (OVERLAY §4.4.2-6): send_hello FIRST, then phase-2
            // validation. On a phase-2 failure we send an (unauthenticated)
            // ERR_CONF and drop — but the HELLO was already on the wire, so the
            // remote can decode the ERROR_MSG.
            let result: Result<()> = async {
                self.send_hello().await?;
                if let Err(e) = self.validate_hello_phase2(&peer_hello) {
                    self.try_send_err_conf(&e).await;
                    return Err(e);
                }
                self.recv_auth(auth_timeout_secs).await?;
                self.send_auth_msg().await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                if self.holds_pending_peer_id {
                    if let Some(ref pending) = pending_peer_ids {
                        pending.remove(&self.info.peer_id);
                    }
                }
                return Err(e);
            }
        }

        self.state = PeerState::Authenticated;
        self.connection.set_authenticated();
        let handshake_ms = handshake_start.elapsed().as_millis();
        info!(
            "Authenticated with peer {} ({}) handshake_ms={}",
            self.info.peer_id, self.info.address, handshake_ms
        );

        // Send SEND_MORE_EXTENDED to enable flow control.
        // Matches stellar-core Peer::recvAuth() → sendSendMore().
        // Both grants are derived from configuration at overlay startup —
        // OVERLAY_SPEC §7.2 (initial SEND_MORE_EXTENDED capacity grant) / §5.4.4.
        let send_more = StellarMessage::SendMoreExtended(stellar_xdr::SendMoreExtended {
            num_messages: initial_message_grant,
            num_bytes: initial_byte_grant,
        });
        self.send(send_more).await?;
        debug!("Sent SEND_MORE_EXTENDED to {}", self.info.peer_id);

        // Ask for SCP data _after_ the flow control message (matches stellar-core recvAuth behavior)
        // Use ledger seq 0 to request the latest SCP state
        let get_scp_state = StellarMessage::GetScpState(0);
        self.send(get_scp_state).await?;
        debug!("Sent GET_SCP_STATE to {}", self.info.peer_id);

        Ok(())
    }

    /// Send our HELLO message (unauthenticated).
    async fn send_hello(&mut self) -> Result<()> {
        let hello = self.auth.create_hello();
        debug!(
            "Sending Hello: overlay_version={}, ledger_version={}, version_str={}, listening_port={}",
            hello.overlay_version,
            hello.ledger_version,
            hello.version_str.to_string(),
            hello.listening_port
        );
        let hello_msg = StellarMessage::Hello(hello);
        self.send_raw(hello_msg).await?;
        self.auth.hello_sent();
        debug!("Hello sent to {}", self.connection.remote_addr());
        Ok(())
    }

    /// Receive the peer's HELLO message and run phase-1 processing.
    ///
    /// Phase-1 = AuthCert verification + X25519 key derivation + the
    /// `HelloReceived` state transition (and peer-identity bookkeeping). It does
    /// NOT run the overlay-version / self-connection / network-ID / port checks —
    /// those are phase-2 ([`validate_hello_phase2`]). On the responder path the
    /// caller echoes its own HELLO between phase-1 and phase-2 so that HELLO
    /// always precedes any ERROR_MSG (OVERLAY §4.4.2-6).
    ///
    /// Returns the received `Hello` so the caller can run phase-2 against it.
    async fn recv_hello(&mut self, timeout_secs: u64) -> Result<Hello> {
        let start = Instant::now();
        let result = self.recv_hello_inner(timeout_secs).await;

        // Parity: stellar-core mRecvHelloTimer (OverlayMetrics.h:46).
        // Record on both success and failure — stellar-core uses an RAII timer scope.
        metrics::histogram!(
            "stellar_overlay_recv_message_seconds",
            "message_type" => "hello"
        )
        .record(start.elapsed().as_secs_f64());

        result
    }

    async fn recv_hello_inner(&mut self, timeout_secs: u64) -> Result<Hello> {
        let frame = self
            .connection
            .recv_timeout(timeout_secs)
            .await?
            .ok_or_else(|| OverlayError::PeerDisconnected("no Hello received".to_string()))?;
        debug!("Received frame with {} bytes", frame.raw_len);
        self.metrics.bytes_read.add(frame.raw_len as u64);
        self.metrics.async_read.inc();

        let message = self.auth.unwrap_message(frame.message)?;

        match message {
            StellarMessage::Hello(peer_hello) => {
                if let Err(e) = self.process_hello_phase1(&peer_hello) {
                    // Best-effort ERR_CONF before dropping, matching
                    // stellar-core Peer::recvHello() → sendErrorAndDrop().
                    self.try_send_err_conf(&e).await;
                    return Err(e);
                }
                Ok(peer_hello)
            }
            other => Err(OverlayError::InvalidMessage(format!(
                "expected Hello, got {}",
                helpers::message_type_name(&other)
            ))),
        }
    }

    /// Best-effort send of ERR_CONF for HELLO failures.
    /// Matches stellar-core Peer::recvHello() error paths that call
    /// sendErrorAndDrop(ERR_CONF, ...) for: wrong network, version mismatch,
    /// self-connection, bad address/port (Peer.cpp:1784-1962).
    async fn try_send_err_conf(&mut self, err: &OverlayError) {
        let reason = match err {
            OverlayError::NetworkMismatch
            | OverlayError::VersionMismatch(_)
            | OverlayError::InvalidMessage(_) => err.to_string(),
            _ => return,
        };
        let truncated = if reason.len() > 100 {
            &reason[..100]
        } else {
            &reason
        };
        let msg = StellarMessage::ErrorMsg(stellar_xdr::SError {
            code: stellar_xdr::ErrorCode::Conf,
            msg: stellar_xdr::StringM::try_from(truncated.to_string()).unwrap_or_default(),
        });
        if let Err(e) = self.send_raw(msg).await {
            debug!("Failed to send ERR_CONF: {}", e);
        }
    }

    /// Send AUTH message (authenticated with MAC, sequence 0).
    async fn send_auth_msg(&mut self) -> Result<()> {
        let auth_msg = StellarMessage::Auth(Auth {
            flags: AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED,
        });
        self.send_auth(auth_msg).await?;
        self.auth.auth_sent();
        debug!("Auth sent to {}", self.connection.remote_addr());
        Ok(())
    }

    /// Receive and process the peer's AUTH message.
    async fn recv_auth(&mut self, timeout_secs: u64) -> Result<()> {
        let start = Instant::now();
        let result = self.recv_auth_inner(timeout_secs).await;

        // Parity: stellar-core mRecvAuthTimer (OverlayMetrics.h:47).
        // Record on both success and failure — stellar-core uses an RAII timer scope.
        metrics::histogram!(
            "stellar_overlay_recv_message_seconds",
            "message_type" => "auth"
        )
        .record(start.elapsed().as_secs_f64());

        result
    }

    async fn recv_auth_inner(&mut self, timeout_secs: u64) -> Result<()> {
        let frame = self
            .connection
            .recv_timeout(timeout_secs)
            .await?
            .ok_or_else(|| OverlayError::PeerDisconnected("no Auth received".to_string()))?;
        self.metrics.bytes_read.add(frame.raw_len as u64);
        self.metrics.async_read.inc();

        let message = self.auth.unwrap_message(frame.message)?;

        match message {
            StellarMessage::Auth(ref auth) => {
                if auth.flags != AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED {
                    return Err(OverlayError::InvalidMessage(format!(
                        "Auth message missing flow control flag, got flags={}",
                        auth.flags
                    )));
                }
                self.auth.process_auth()?;
            }
            StellarMessage::ErrorMsg(err) => {
                // OVERLAY §7.1.3-1: sanitize raw message bytes before logging /
                // storing (NOT `to_string()`, which escapes instead of
                // collapsing non-printable bytes to `*`).
                let err_msg: String = sanitize_error_msg(&err.msg[..]);
                warn!("Peer sent error: code={:?}, msg={}", err.code, err_msg);
                return Err(OverlayError::InvalidMessage(format!(
                    "peer sent ERROR: code={:?}, msg={}",
                    err.code, err_msg
                )));
            }
            other => {
                return Err(OverlayError::InvalidMessage(format!(
                    "expected Auth, got {}",
                    helpers::message_type_name(&other)
                )));
            }
        }

        Ok(())
    }
    /// Phase-1 of peer-level HELLO processing.
    ///
    /// Verifies the AuthCert, derives MAC keys, transitions the auth state to
    /// `HelloReceived` (via [`AuthContext::process_hello_phase1`]), and records
    /// the peer identity / advertised versions on `self.info` so the post-HELLO
    /// ban check has the peer id available.
    ///
    /// Deliberately does NOT run the overlay-version / self-connection /
    /// network-ID / port checks — those are deferred to [`validate_hello_phase2`]
    /// so the responder can echo its HELLO first (OVERLAY §4.4.2-6).
    fn process_hello_phase1(&mut self, hello: &Hello) -> Result<()> {
        // State guard: reject if not in Handshaking state
        if self.state != PeerState::Handshaking {
            return Err(OverlayError::InvalidMessage(format!(
                "received Hello in unexpected state {:?}",
                self.state
            )));
        }

        // AuthCert verify + X25519 key derivation + HelloReceived state.
        self.auth.process_hello_phase1(hello)?;

        // Extract peer info
        let peer_id = self
            .auth
            .peer_id()
            .cloned()
            .ok_or_else(|| OverlayError::AuthenticationFailed("no peer ID".to_string()))?;

        self.info.peer_id = peer_id;
        self.info.version_string = hello.version_str.to_string();
        self.info.overlay_version = hello.overlay_version;
        self.info.ledger_version = hello.ledger_version;

        debug!(
            "Received Hello from {} (version: {}, overlay: {})",
            self.info.peer_id, self.info.version_string, self.info.overlay_version
        );

        Ok(())
    }

    /// Phase-2 of peer-level HELLO processing — the validation checks that, on
    /// the responder path, run AFTER the local HELLO has been echoed.
    ///
    /// Check order matches stellar-core `Peer::recvHello`: overlay version →
    /// self-connection → network ID → listening port. On the first failure the
    /// caller emits an (unauthenticated) ERR_CONF and drops the connection.
    fn validate_hello_phase2(&mut self, hello: &Hello) -> Result<()> {
        // 1. Overlay-version range + network-ID checks (auth-level).
        //    Note: validate_hello_post_send checks version BEFORE network, so
        //    the version error takes precedence — matching stellar-core order.
        //    The network check is the last of the auth-level checks; the
        //    self-connection check (peer-level) is interposed between them
        //    below to mirror stellar-core's version → self → network ordering.
        self.auth.validate_overlay_version(hello)?;

        // 2. Self-connection check: reject if peer is ourselves.
        let local_peer_id = self.auth.local_peer_id();
        if self.info.peer_id == local_peer_id {
            // "connecting to self" mirrors stellar-core Peer.cpp:1857 /
            // OVERLAY_SPEC §4.4.2-8 (the exact on-the-wire handshake error).
            return Err(OverlayError::InvalidMessage(
                "connecting to self".to_string(),
            ));
        }

        // 3. Network-ID check (auth-level).
        self.auth.validate_network_id(hello)?;

        // 4. Listening-port validation: XDR uses i32, but valid ports are
        //    1-65535. Reject port 0 — matches stellar-core Peer::recvHello()
        //    which rejects listeningPort <= 0 to prevent poisoning peer gossip
        //    with ephemeral ports.
        if hello.listening_port <= 0 || hello.listening_port > u16::MAX as i32 {
            // "bad address" mirrors stellar-core Peer.cpp:1875 / OVERLAY_SPEC
            // §4.4.2-10 (the exact on-the-wire handshake error string).
            return Err(OverlayError::InvalidMessage("bad address".to_string()));
        }

        // All checks passed — record the peer's advertised listening port.
        let port = hello.listening_port as u16;
        let ip = self.info.address.ip();
        self.info.address = SocketAddr::new(ip, port);

        Ok(())
    }

    /// Send a raw message (before authentication, e.g., Hello).
    async fn send_raw(&mut self, message: StellarMessage) -> Result<()> {
        let kind = OverlayMessageKind::from_stellar_message(&message);
        // #3419 diagnostic: record the last-sent wire type (observability-only).
        self.last_sent_msg_type = helpers::message_type_name(&message);
        let body_size = msg_body_size(&message);
        let msg_type = self.last_sent_msg_type;
        let auth_msg = self.auth.wrap_unauthenticated(message);
        // `Connection::send` returns the on-the-wire frame size plus a byte
        // prefix (#3773), so we don't re-encode here just to measure it.
        let outcome = self.connection.send(auth_msg).await?;
        // Success-only instrumentation: connection errors go to errors_write at
        // the caller (peer_loop), not bytes_written/async_write.
        self.metrics.record_send(kind);
        self.metrics.bytes_written.add(outcome.wire_size);
        self.metrics.async_write.inc();
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(body_size, Ordering::Relaxed);
        self.record_recent_send(msg_type, outcome.wire_size, outcome.prefix);
        Ok(())
    }

    /// Send an Auth message (with MAC but sequence 0).
    async fn send_auth(&mut self, message: StellarMessage) -> Result<()> {
        let kind = OverlayMessageKind::from_stellar_message(&message);
        // #3419 diagnostic: record the last-sent wire type (observability-only).
        self.last_sent_msg_type = helpers::message_type_name(&message);
        let msg_type = self.last_sent_msg_type;
        let body_size = msg_body_size(&message);
        let auth_msg = self.auth.wrap_auth_message(message)?;
        let outcome = self.connection.send(auth_msg).await?;
        self.metrics.record_send(kind);
        self.metrics.bytes_written.add(outcome.wire_size);
        self.metrics.async_write.inc();
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(body_size, Ordering::Relaxed);
        self.record_recent_send(msg_type, outcome.wire_size, outcome.prefix);
        Ok(())
    }

    /// Send a message to this peer.
    pub async fn send(&mut self, message: StellarMessage) -> Result<()> {
        if self.state != PeerState::Authenticated {
            return Err(OverlayError::PeerDisconnected(
                "not authenticated".to_string(),
            ));
        }

        let kind = OverlayMessageKind::from_stellar_message(&message);
        let msg_type = helpers::message_type_name(&message);
        // #3419 diagnostic: record the last-sent wire type (observability-only).
        self.last_sent_msg_type = msg_type;
        trace!("SEND {} to {}", msg_type, self.info.peer_id);

        let body_size = msg_body_size(&message);
        let auth_msg = self.auth.wrap_message(message)?;
        let outcome = self.connection.send(auth_msg).await?;
        self.metrics.record_send(kind);
        self.metrics.bytes_written.add(outcome.wire_size);
        self.metrics.async_write.inc();
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(body_size, Ordering::Relaxed);
        self.record_recent_send(msg_type, outcome.wire_size, outcome.prefix);

        Ok(())
    }

    /// maxtps (T2): send a batch of messages in a SINGLE coalesced write.
    /// Wraps + encodes each message in order (preserving the auth sequence),
    /// concatenates them, and issues one `Connection::send_encoded_batch` —
    /// collapsing N syscalls/TCP-segments into one. Per-message metrics/stats
    /// are applied after a successful write. On error nothing was sent (the
    /// whole buffer is one `write_all`), and the peer is dropped by the caller.
    pub async fn send_batch(&mut self, messages: &[StellarMessage]) -> Result<()> {
        if self.state != PeerState::Authenticated {
            return Err(OverlayError::PeerDisconnected(
                "not authenticated".to_string(),
            ));
        }
        if messages.is_empty() {
            return Ok(());
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut kinds = Vec::with_capacity(messages.len());
        // #3773: per-message (type, wire_size, byte-prefix) captured from the
        // same local `encoded` buffer we concatenate for the write — no extra
        // encode pass, no wire mutation. Recorded onto the ring only AFTER the
        // batch write succeeds (success-only, like the other send paths).
        let mut recent: Vec<(&'static str, u64, [u8; crate::connection::SEND_PREFIX_LEN])> =
            Vec::with_capacity(messages.len());
        let mut total_body_size = 0u64;
        let mut total_wire_size = 0u64;
        for message in messages {
            let kind = OverlayMessageKind::from_stellar_message(message);
            let msg_type = helpers::message_type_name(message);
            self.last_sent_msg_type = msg_type;
            total_body_size += msg_body_size(message);
            let auth_msg = self.auth.wrap_message(message.clone())?;
            let encoded = crate::codec::MessageCodec::encode_message(&auth_msg)?;
            let wire_size = (encoded.len() - 4) as u64;
            total_wire_size += wire_size;
            recent.push((
                msg_type,
                wire_size,
                crate::connection::encoded_body_prefix(&encoded),
            ));
            buf.extend_from_slice(&encoded);
            kinds.push(kind);
        }

        self.connection.send_encoded_batch(&buf).await?;

        for kind in kinds {
            self.metrics.record_send(kind);
        }
        self.metrics.bytes_written.add(total_wire_size);
        self.metrics.async_write.inc();
        self.stats
            .messages_sent
            .fetch_add(messages.len() as u64, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(total_body_size, Ordering::Relaxed);
        for (msg_type, wire_size, prefix) in recent {
            self.record_recent_send(msg_type, wire_size, prefix);
        }
        Ok(())
    }

    /// Receive a message from this peer.
    pub async fn recv(&mut self) -> Result<Option<StellarMessage>> {
        if self.state != PeerState::Authenticated {
            return Ok(None);
        }

        let frame = match self.connection.recv().await? {
            Some(f) => f,
            None => {
                self.state = PeerState::Disconnected;
                return Ok(None);
            }
        };

        // Success-only instrumentation: a frame was successfully decoded from
        // the wire. Decode failures surface as `Err` from `connection.recv()`
        // and are counted as `errors_read` by the peer loop.
        self.metrics.bytes_read.add(frame.raw_len as u64);
        self.metrics.async_read.inc();
        self.stats.messages_received.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_received
            .fetch_add(frame.raw_len as u64, Ordering::Relaxed);

        let message = self.auth.unwrap_message(frame.message)?;
        let msg_type = helpers::message_type_name(&message);
        trace!("Received {} from {}", msg_type, self.info.peer_id);

        Ok(Some(message))
    }

    /// Receive a message with timeout.
    pub async fn recv_timeout(&mut self, timeout_secs: u64) -> Result<Option<StellarMessage>> {
        if self.state != PeerState::Authenticated {
            return Ok(None);
        }

        let frame = match self.connection.recv_timeout(timeout_secs).await? {
            Some(f) => f,
            None => {
                self.state = PeerState::Disconnected;
                return Ok(None);
            }
        };

        self.metrics.bytes_read.add(frame.raw_len as u64);
        self.metrics.async_read.inc();
        self.stats.messages_received.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_received
            .fetch_add(frame.raw_len as u64, Ordering::Relaxed);

        let message = self.auth.unwrap_message(frame.message)?;

        Ok(Some(message))
    }

    /// Get this peer's ID.
    pub fn id(&self) -> &PeerId {
        &self.info.peer_id
    }

    /// Get peer info.
    pub fn info(&self) -> &PeerInfo {
        &self.info
    }

    /// Get current state.
    pub fn state(&self) -> PeerState {
        self.state
    }

    /// Check if this peer is still connected.
    pub fn is_connected(&self) -> bool {
        self.state.is_connected()
    }

    /// Check if this peer is ready for messages.
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// Get statistics.
    pub fn stats(&self) -> Arc<PeerStats> {
        Arc::clone(&self.stats)
    }

    fn record_message_stats(
        &self,
        unique: bool,
        bytes: u64,
        unique_msgs: &AtomicU64,
        unique_bytes: &AtomicU64,
        dup_msgs: &AtomicU64,
        dup_bytes: &AtomicU64,
    ) {
        if unique {
            unique_msgs.fetch_add(1, Ordering::Relaxed);
            unique_bytes.fetch_add(bytes, Ordering::Relaxed);
        } else {
            dup_msgs.fetch_add(1, Ordering::Relaxed);
            dup_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub fn record_flood_stats(&self, unique: bool, bytes: u64) {
        self.record_message_stats(
            unique,
            bytes,
            &self.stats.unique_flood_messages_recv,
            &self.stats.unique_flood_bytes_recv,
            &self.stats.duplicate_flood_messages_recv,
            &self.stats.duplicate_flood_bytes_recv,
        );
    }

    pub fn record_fetch_stats(&self, unique: bool, bytes: u64) {
        self.record_message_stats(
            unique,
            bytes,
            &self.stats.unique_fetch_messages_recv,
            &self.stats.unique_fetch_bytes_recv,
            &self.stats.duplicate_fetch_messages_recv,
            &self.stats.duplicate_fetch_bytes_recv,
        );
    }

    /// Get remote address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.info.address
    }

    /// Get connection direction.
    pub fn direction(&self) -> ConnectionDirection {
        self.info.direction
    }

    /// Diagnostic (#3419, observability-only): the wire name of the most-recent
    /// `StellarMessage` sent on this connection, or `"none"` if nothing has been
    /// sent yet. Read by the peer loop on drop to surface the last message
    /// henyey sent before a remote reset.
    pub fn last_sent_msg_type(&self) -> &'static str {
        self.last_sent_msg_type
    }

    /// #3773: push one outbound-send record onto the bounded diagnostic ring,
    /// evicting the oldest entry once at [`RECENT_SENDS_CAPACITY`]. Called on
    /// the success path of every send method. Diagnostic-only — never gates
    /// any logic and never touches the wire.
    fn record_recent_send(
        &mut self,
        msg_type: &'static str,
        wire_size: u64,
        prefix: [u8; crate::connection::SEND_PREFIX_LEN],
    ) {
        if self.recent_sends.len() == RECENT_SENDS_CAPACITY {
            self.recent_sends.pop_front();
        }
        self.recent_sends.push_back(RecentSend {
            at: Instant::now(),
            msg_type,
            wire_size,
            prefix,
        });
    }

    /// #3773: human-readable dump of the outbound diagnostic ring, logged next
    /// to the `Peer sent_error` warning so an operator can see which frame(s)
    /// henyey sent immediately before a peer rejected our encoding with
    /// `ERR_DATA "received corrupt XDR"`. Entries are oldest-first; each shows
    /// the wire type, body size, age at read time, and the hex byte prefix.
    /// Returns `"none"` when nothing has been sent yet.
    pub(crate) fn recent_sends_summary(&self) -> String {
        if self.recent_sends.is_empty() {
            return "none".to_string();
        }
        let now = Instant::now();
        let entries: Vec<String> = self
            .recent_sends
            .iter()
            .map(|s| {
                format!(
                    "{}(size={},age_ms={},prefix={})",
                    s.msg_type,
                    s.wire_size,
                    now.saturating_duration_since(s.at).as_millis(),
                    hex::encode(s.prefix)
                )
            })
            .collect();
        format!("{} sends: {}", entries.len(), entries.join("; "))
    }

    /// Whether this peer owns a pending_peer_id reservation.
    /// Used by the manager to decide whether to call `release_peer_id`
    /// during cleanup — peers that bypassed the reservation in a
    /// mutual-dial scenario must not release the outbound reservation.
    pub fn holds_pending_peer_id(&self) -> bool {
        self.holds_pending_peer_id
    }

    /// Request SCP state from peer.
    pub async fn request_scp_state(&mut self, ledger_seq: u32) -> Result<()> {
        let message = StellarMessage::GetScpState(ledger_seq);
        self.send(message).await
    }

    /// Request peers from this peer.
    /// Note: GetPeers was removed in Protocol 24. This is a no-op.
    /// Send extended flow control message with byte limit.
    pub async fn send_more_extended(&mut self, num_messages: u32, num_bytes: u32) -> Result<()> {
        let message = StellarMessage::SendMoreExtended(stellar_xdr::SendMoreExtended {
            num_messages,
            num_bytes,
        });
        self.send(message).await
    }

    /// Close the connection.
    pub async fn close(&mut self) {
        if self.state != PeerState::Disconnected {
            self.state = PeerState::Closing;
            self.connection.close().await;
            self.state = PeerState::Disconnected;
            debug!("Closed connection to {}", self.info.peer_id);
        }
    }

    /// Construct a fake inbound peer for testing cleanup paths only.
    ///
    /// The peer is in `Authenticated` state but has no real auth keys —
    /// only suitable for tests that exercise early-return rejection logic
    /// (banned, duplicate, pool-full) without sending or receiving messages.
    #[cfg(test)]
    pub(crate) fn new_test_inbound(
        peer_id: PeerId,
        holds_pending_peer_id: bool,
        metrics: Arc<OverlayMetrics>,
    ) -> Self {
        use crate::auth::AuthContext;
        use crate::connection::Connection;
        use henyey_crypto::SecretKey;

        let (client, _server) = tokio::io::duplex(1024);
        let addr: std::net::SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let conn = Connection::from_io(client, addr, ConnectionDirection::Inbound).unwrap();
        let local = LocalNode::new_testnet(SecretKey::generate());
        let auth = AuthContext::new(local, false);

        Self {
            info: PeerInfo {
                peer_id,
                address: addr,
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Authenticated,
            connection: conn,
            auth,
            stats: Arc::new(PeerStats::default()),
            metrics,
            holds_pending_peer_id,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        }
    }

    /// Construct a fully-authenticated peer pair with **real derived MAC keys**,
    /// for tests that must exercise the post-auth send paths (`send`,
    /// `send_auth`, `send_batch`) which call `wrap_message`/`wrap_auth_message`
    /// and therefore require derived keys. The byte-counter pair helper in the
    /// test module never derives keys, so it can only drive the unauthenticated
    /// `send_raw` path.
    ///
    /// Both peers share a duplex transport and complete a full HELLO + AUTH
    /// handshake at the `AuthContext` layer (mirroring auth.rs's
    /// `complete_handshake`), leaving `send_mac_key`/`recv_mac_key` populated
    /// and sequence counters aligned. Returned in `Authenticated` state. The
    /// caller must keep BOTH peers alive so neither duplex half is dropped.
    #[cfg(test)]
    pub(crate) fn new_test_authenticated_pair(
        metrics_a: Arc<OverlayMetrics>,
        metrics_b: Arc<OverlayMetrics>,
    ) -> (Self, Self) {
        use crate::auth::AuthContext;
        use crate::connection::Connection;
        use henyey_crypto::SecretKey;

        let (client, server) = tokio::io::duplex(1024 * 1024);
        let addr_a: SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:11626".parse().unwrap();
        let conn_a = Connection::from_io(client, addr_a, ConnectionDirection::Outbound).unwrap();
        let conn_b = Connection::from_io(server, addr_b, ConnectionDirection::Inbound).unwrap();

        let node_a = LocalNode::new_testnet(SecretKey::generate());
        let node_b = LocalNode::new_testnet(SecretKey::generate());
        let mut auth_a = AuthContext::new(node_a, true);
        let mut auth_b = AuthContext::new(node_b, false);

        // Full HELLO + AUTH handshake so both sides derive MAC keys and align
        // sequence counters.
        let hello_a = auth_a.create_hello();
        let hello_b = auth_b.create_hello();
        auth_a.hello_sent();
        auth_b.hello_sent();
        auth_b.process_hello(&hello_a).expect("B accepts A hello");
        auth_a.process_hello(&hello_b).expect("A accepts B hello");
        let am_a = auth_a
            .wrap_auth_message(StellarMessage::Auth(Auth {
                flags: AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED,
            }))
            .expect("A wraps auth");
        let am_b = auth_b
            .wrap_auth_message(StellarMessage::Auth(Auth {
                flags: AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED,
            }))
            .expect("B wraps auth");
        auth_a.auth_sent();
        auth_b.auth_sent();
        auth_b.unwrap_message(am_a).expect("B unwraps A auth");
        auth_a.unwrap_message(am_b).expect("A unwraps B auth");
        auth_a.process_auth().expect("A completes auth");
        auth_b.process_auth().expect("B completes auth");

        let peer_a = Self {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([1u8; 32]),
                address: addr_b,
                direction: ConnectionDirection::Outbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Authenticated,
            connection: conn_a,
            auth: auth_a,
            stats: Arc::new(PeerStats::default()),
            metrics: metrics_a,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };
        let peer_b = Self {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([2u8; 32]),
                address: addr_a,
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Authenticated,
            connection: conn_b,
            auth: auth_b,
            stats: Arc::new(PeerStats::default()),
            metrics: metrics_b,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };
        (peer_a, peer_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_state() {
        assert!(!PeerState::Connecting.is_connected());
        assert!(PeerState::Handshaking.is_connected());
        assert!(PeerState::Authenticated.is_connected());
        assert!(PeerState::Authenticated.is_ready());
        assert!(!PeerState::Handshaking.is_ready());
        assert!(!PeerState::Disconnected.is_connected());
    }

    #[test]
    fn test_peer_stats() {
        let stats = PeerStats::default();
        stats.messages_sent.fetch_add(10, Ordering::Relaxed);
        stats.messages_received.fetch_add(5, Ordering::Relaxed);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.messages_sent, 10);
        assert_eq!(snapshot.messages_received, 5);
    }

    /// Construct a `Peer` directly without going through a real handshake,
    /// pre-set to `Authenticated` so `send()` and `recv()` will run their
    /// instrumented bodies. Used by the byte/async-counter tests below.
    fn make_authenticated_peer_pair(
        metrics_a: Arc<OverlayMetrics>,
        metrics_b: Arc<OverlayMetrics>,
    ) -> (Peer, Peer) {
        use crate::auth::AuthContext;
        use crate::connection::Connection;
        use henyey_crypto::SecretKey;

        let (client, server) = tokio::io::duplex(1024 * 1024);
        let addr_a: SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:11626".parse().unwrap();
        let conn_a = Connection::from_io(client, addr_a, ConnectionDirection::Outbound).unwrap();
        let conn_b = Connection::from_io(server, addr_b, ConnectionDirection::Inbound).unwrap();

        let local_a = LocalNode::new_testnet(SecretKey::generate());
        let local_b = LocalNode::new_testnet(SecretKey::generate());
        // Cross-wire the peers' AuthContexts so `wrap_unauthenticated` /
        // `unwrap_message` agree on the unauthenticated framing. Authenticated
        // frames are not exchanged here because `state == Authenticated` does
        // not actually mean the MAC keys are derived — for that we'd need a
        // full handshake. The tests in this module that call `peer.send()`
        // therefore restrict themselves to the *unauthenticated* path
        // (i.e., directly drive `Connection::send` via `Peer::send_raw`).
        let auth_a = AuthContext::new(local_a, true);
        let auth_b = AuthContext::new(local_b, false);

        let peer_a = Peer {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: addr_b,
                direction: ConnectionDirection::Outbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Authenticated,
            connection: conn_a,
            auth: auth_a,
            stats: Arc::new(PeerStats::default()),
            metrics: metrics_a,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };
        let peer_b = Peer {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: addr_a,
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Authenticated,
            connection: conn_b,
            auth: auth_b,
            stats: Arc::new(PeerStats::default()),
            metrics: metrics_b,
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };
        (peer_a, peer_b)
    }

    /// Build a single inbound `Peer` in `Handshaking` state plus a valid
    /// `Hello` (correct network + in-range overlay version) crafted by a
    /// remote node, so `validate_hello_phase2` reaches the peer-level
    /// self-connection / bad-address checks. Returns the peer and the hello.
    fn make_peer_for_phase2() -> (Peer, Hello) {
        use crate::auth::{AuthCert, AuthCertExt, AuthContext};
        use crate::connection::Connection;
        use henyey_crypto::SecretKey;
        use stellar_xdr as xdr;
        use x25519_dalek::EphemeralSecret;

        let (_client, server) = tokio::io::duplex(1024 * 1024);
        let addr_local: SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let addr_remote: SocketAddr = "127.0.0.1:11626".parse().unwrap();
        let conn = Connection::from_io(server, addr_remote, ConnectionDirection::Inbound).unwrap();

        let local = LocalNode::new_testnet(SecretKey::generate());
        let remote = LocalNode::new_testnet(SecretKey::generate());
        let auth = AuthContext::new(local, false);

        // A hello from the remote node: same network + matching overlay version
        // range (both testnet nodes share these), so the auth-level checks pass
        // and `validate_hello_phase2` reaches the peer-level checks.
        let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let cert = AuthCert::new_cert(&remote, &ephemeral);
        let hello = Hello {
            ledger_version: 25,
            overlay_version: remote.overlay_version,
            overlay_min_version: remote.overlay_min_version,
            network_id: xdr::Hash(*remote.network_id.as_bytes()),
            version_str: remote.version_string.clone().try_into().unwrap(),
            listening_port: remote.listening_port as i32,
            peer_id: xdr::NodeId(remote.xdr_public_key()),
            cert,
            nonce: xdr::Uint256([42u8; 32]),
        };

        let peer = Peer {
            info: PeerInfo {
                peer_id: PeerId::from_xdr(remote.xdr_public_key()),
                address: addr_local,
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Handshaking,
            connection: conn,
            auth,
            stats: Arc::new(PeerStats::default()),
            metrics: Arc::new(OverlayMetrics::new()),
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };
        (peer, hello)
    }

    /// §4.4.2-8 / Peer.cpp:1857: the self-connection rejection error string
    /// must be exactly "connecting to self" (issue #3080).
    #[test]
    fn test_self_connection_error_string_matches_spec() {
        let (mut peer, hello) = make_peer_for_phase2();
        // Force the self-connection branch: recorded peer id == our own id.
        peer.info.peer_id = peer.auth.local_peer_id();

        let err = peer.validate_hello_phase2(&hello).unwrap_err();
        assert_eq!(err.to_string(), "invalid message: connecting to self");
    }

    /// §4.4.2-10 / Peer.cpp:1875: the bad-address rejection error string must
    /// be exactly "bad address" (issue #3080).
    #[test]
    fn test_bad_address_error_string_matches_spec() {
        let (mut peer, mut hello) = make_peer_for_phase2();
        hello.listening_port = 0; // invalid — triggers the bad-address check

        let err = peer.validate_hello_phase2(&hello).unwrap_err();
        assert_eq!(err.to_string(), "invalid message: bad address");
    }

    /// Verify that the byte and async I/O counters are zero on a fresh peer
    /// — sanity check that adding the new fields didn't accidentally
    /// initialize them with non-zero values.
    #[test]
    fn test_metrics_default_zero() {
        let m = OverlayMetrics::new();
        assert_eq!(m.bytes_read.get(), 0);
        assert_eq!(m.bytes_written.get(), 0);
        assert_eq!(m.async_read.get(), 0);
        assert_eq!(m.async_write.get(), 0);
        assert_eq!(m.inbound_attempt.get(), 0);
        assert_eq!(m.inbound_establish.get(), 0);
        assert_eq!(m.inbound_drop.get(), 0);
        assert_eq!(m.inbound_reject.get(), 0);
        assert_eq!(m.outbound_attempt.get(), 0);
        assert_eq!(m.outbound_establish.get(), 0);
        assert_eq!(m.outbound_drop.get(), 0);
        assert_eq!(m.outbound_reject.get(), 0);
    }

    /// Drive a `Peer::send_hello` -> `Peer::recv_hello` round-trip and assert
    /// that BOTH sides update their counters via the real instrumented paths
    /// (no manual counter bumps). A regression in either `send_raw`'s or
    /// `recv_hello`'s instrumentation would cause this test to fail.
    ///
    /// Asserts:
    ///   - A's `bytes_written` / `async_write` increment by wire size / 1
    ///   - B's `bytes_read` / `async_read` increment by the same wire size / 1
    #[tokio::test]
    async fn test_peer_send_recv_metrics_increment() {
        // We drive `send_hello` (unauthenticated send_raw) on A and
        // `recv_hello` on B (instrumented receive). We can't use the
        // post-auth `send`/`recv` here because the test peers don't have
        // derived MAC keys without a full handshake — `recv_hello` only
        // requires unwrap of an unauthenticated frame.
        let metrics_a = Arc::new(OverlayMetrics::new());
        let metrics_b = Arc::new(OverlayMetrics::new());
        let (mut peer_a, mut peer_b) =
            make_authenticated_peer_pair(Arc::clone(&metrics_a), Arc::clone(&metrics_b));

        // Send a HELLO via the real `send_hello` (which calls send_raw) and
        // receive it on B via the real `recv_hello`. Both paths run their
        // metric instrumentation; a regression in either would break this
        // assertion.
        peer_a.send_hello().await.expect("send_hello");
        // `recv_hello` will fail at `process_hello` (network mismatch /
        // self-cert checks) for our synthetic peer pair, but the
        // instrumentation runs BEFORE process_hello — so we accept either
        // outcome here and only assert on the counter side-effects below.
        let _ = peer_b.recv_hello(5).await;

        // Sender side: bytes_written / async_write incremented exactly once
        // by the real `send_raw` instrumentation.
        assert_eq!(metrics_a.async_write.get(), 1);
        assert!(
            metrics_a.bytes_written.get() > 0,
            "expected bytes_written > 0, got {}",
            metrics_a.bytes_written.get()
        );

        // Receiver side: counts the same wire bytes via the real
        // `recv_hello` instrumentation.
        assert_eq!(
            metrics_b.async_read.get(),
            1,
            "recv_hello must bump async_read exactly once"
        );
        assert_eq!(
            metrics_b.bytes_read.get(),
            metrics_a.bytes_written.get(),
            "wire-level byte counts must match between sender and receiver \
             (both sides must use real instrumentation)"
        );
    }

    /// Verify that a failed send does NOT increment success-only counters.
    ///
    /// We force a deterministic failure by closing peer_a's own connection
    /// BEFORE attempting the send: `Connection::send` has an early return
    /// path that yields `PeerDisconnected` when `self.closed` is true, so
    /// the send is guaranteed to fail without depending on duplex-buffer
    /// timing.
    #[tokio::test]
    async fn test_peer_failed_send_does_not_increment_counters() {
        let metrics_a = Arc::new(OverlayMetrics::new());
        let metrics_b = Arc::new(OverlayMetrics::new());
        let (mut peer_a, _peer_b) =
            make_authenticated_peer_pair(Arc::clone(&metrics_a), Arc::clone(&metrics_b));

        // Close peer_a's connection. `Connection::send` will return
        // `PeerDisconnected` immediately on the next call (deterministic).
        peer_a.connection.close().await;

        let result = peer_a.send_hello().await;
        assert!(
            result.is_err(),
            "send_hello must fail deterministically when local connection is closed, got: {:?}",
            result
        );

        // Success-only counters must remain at zero on the failure path.
        assert_eq!(
            metrics_a.async_write.get(),
            0,
            "async_write must not increment on failed send"
        );
        assert_eq!(
            metrics_a.bytes_written.get(),
            0,
            "bytes_written must not increment on failed send"
        );
    }

    /// Verify reset() includes the new Stage F.1 fields.
    #[test]
    fn test_metrics_reset_clears_stage_f1_fields() {
        let m = OverlayMetrics::new();
        m.bytes_read.add(100);
        m.bytes_written.add(200);
        m.async_read.inc();
        m.async_write.inc();
        m.inbound_attempt.inc();
        m.inbound_establish.inc();
        m.inbound_drop.inc();
        m.inbound_reject.inc();
        m.outbound_attempt.inc();
        m.outbound_establish.inc();
        m.outbound_drop.inc();
        m.outbound_reject.inc();

        m.reset();

        assert_eq!(m.bytes_read.get(), 0);
        assert_eq!(m.bytes_written.get(), 0);
        assert_eq!(m.async_read.get(), 0);
        assert_eq!(m.async_write.get(), 0);
        assert_eq!(m.inbound_attempt.get(), 0);
        assert_eq!(m.inbound_establish.get(), 0);
        assert_eq!(m.inbound_drop.get(), 0);
        assert_eq!(m.inbound_reject.get(), 0);
        assert_eq!(m.outbound_attempt.get(), 0);
        assert_eq!(m.outbound_establish.get(), 0);
        assert_eq!(m.outbound_drop.get(), 0);
        assert_eq!(m.outbound_reject.get(), 0);
    }

    /// Verify the snapshot includes all new Stage F.1 fields.
    #[test]
    fn test_metrics_snapshot_includes_stage_f1_fields() {
        let m = OverlayMetrics::new();
        m.bytes_read.add(123);
        m.bytes_written.add(456);
        m.async_read.add(7);
        m.async_write.add(8);
        m.inbound_attempt.add(11);
        m.inbound_establish.add(12);
        m.inbound_drop.add(13);
        m.inbound_reject.add(14);
        m.outbound_attempt.add(21);
        m.outbound_establish.add(22);
        m.outbound_drop.add(23);
        m.outbound_reject.add(24);

        let snap = m.snapshot();

        assert_eq!(snap.bytes_read, 123);
        assert_eq!(snap.bytes_written, 456);
        assert_eq!(snap.async_read, 7);
        assert_eq!(snap.async_write, 8);
        assert_eq!(snap.inbound_attempt, 11);
        assert_eq!(snap.inbound_establish, 12);
        assert_eq!(snap.inbound_drop, 13);
        assert_eq!(snap.inbound_reject, 14);
        assert_eq!(snap.outbound_attempt, 21);
        assert_eq!(snap.outbound_establish, 22);
        assert_eq!(snap.outbound_drop, 23);
        assert_eq!(snap.outbound_reject, 24);
    }

    // ── OVERLAY §4.4.2-6: responder sends HELLO before ERROR_MSG (#3067) ──

    use crate::auth::AuthContext;
    use crate::connection::Connection;
    use henyey_crypto::SecretKey;
    use stellar_xdr::Hello;

    /// Build a responder `Peer` (inbound, `Handshaking`) wired over an in-memory
    /// duplex to a raw client-side `(Connection, AuthContext)` initiator pair.
    /// The responder is driven via `handshake()`; the client crafts and sends a
    /// HELLO, then reads the responder's reply frames.
    fn make_responder_and_client(client_local: LocalNode) -> (Peer, Connection, AuthContext) {
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let client_addr: SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let server_addr: SocketAddr = "127.0.0.1:11626".parse().unwrap();

        let client_conn =
            Connection::from_io(client_io, server_addr, ConnectionDirection::Outbound).unwrap();
        let server_conn =
            Connection::from_io(server_io, client_addr, ConnectionDirection::Inbound).unwrap();

        let responder_local = LocalNode::new_testnet(SecretKey::generate());
        let responder_auth = AuthContext::new(responder_local, false); // they called us
        let client_auth = AuthContext::new(client_local, true); // we called remote

        let responder = Peer {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: client_addr,
                direction: ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Connecting,
            connection: server_conn,
            auth: responder_auth,
            stats: Arc::new(PeerStats::default()),
            metrics: Arc::new(OverlayMetrics::new()),
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };

        (responder, client_conn, client_auth)
    }

    /// Drive the responder handshake against a client that sends `hello`, then
    /// collect the StellarMessages the responder sent back (in order) until the
    /// connection closes or two frames have been read.
    async fn responder_reply_frames(
        client_local: LocalNode,
        mutate_hello: impl FnOnce(&mut Hello),
    ) -> Vec<StellarMessage> {
        let (mut responder, mut client_conn, mut client_auth) =
            make_responder_and_client(client_local);

        // Build the client's HELLO and apply the test mutation.
        let mut hello = client_auth.create_hello();
        mutate_hello(&mut hello);

        // Spawn the responder's full handshake. It will receive HELLO, run
        // phase-1, ban check, send_hello(), then phase-2 — which fails for the
        // mismatched HELLO, sends ERR_CONF, and drops.
        let responder_task = tokio::spawn(async move {
            let res = responder.handshake(5, None, None, 0, 0).await;
            // Return the result so the test can assert the responder dropped.
            res
        });

        // Client sends its (mismatched) HELLO unauthenticated.
        let hello_frame = client_auth.wrap_unauthenticated(StellarMessage::Hello(hello));
        client_conn
            .send(hello_frame)
            .await
            .expect("client send hello");

        // Read frames the responder sends back. Expect HELLO then ERR_CONF.
        let mut frames = Vec::new();
        for _ in 0..2 {
            match client_conn.recv_timeout(5).await {
                Ok(Some(frame)) => {
                    // Pre-auth framing: unwrap without MAC enforcement.
                    let msg = client_auth
                        .unwrap_message(frame.message)
                        .expect("unwrap responder frame");
                    frames.push(msg);
                }
                _ => break,
            }
        }

        // The responder must have dropped with an error (not authenticated).
        let res = responder_task.await.expect("responder task join");
        assert!(
            res.is_err(),
            "responder must drop after a mismatched HELLO, got {:?}",
            res
        );

        frames
    }

    #[tokio::test]
    async fn test_responder_sends_hello_before_error_on_version_mismatch() {
        // Regression for #3067: on an overlay-version mismatch the responder
        // MUST send its HELLO before the ERROR_MSG(Conf). On main the version
        // check runs before send_hello(), so the first (and only) frame is the
        // ERR_CONF — this test FAILS on main and PASSES after the reorder.
        let client_local = LocalNode::new_testnet(SecretKey::generate());
        let frames = responder_reply_frames(client_local, |hello| {
            // Advertise an incompatible (too-old) overlay version range.
            hello.overlay_version = 5;
            hello.overlay_min_version = 1;
        })
        .await;

        assert!(
            frames.len() >= 2,
            "responder must send HELLO then ERROR, got {} frame(s): {:?}",
            frames.len(),
            frames
                .iter()
                .map(helpers::message_type_name)
                .collect::<Vec<_>>()
        );
        assert!(
            matches!(frames[0], StellarMessage::Hello(_)),
            "first frame must be HELLO, got {}",
            helpers::message_type_name(&frames[0])
        );
        match &frames[1] {
            StellarMessage::ErrorMsg(e) => {
                assert_eq!(
                    e.code,
                    stellar_xdr::ErrorCode::Conf,
                    "second frame must be ERR_CONF"
                );
            }
            other => panic!(
                "second frame must be ERROR_MSG, got {}",
                helpers::message_type_name(other)
            ),
        }
    }

    #[tokio::test]
    async fn test_responder_sends_hello_before_error_on_network_mismatch() {
        // Regression for #3067: same ordering requirement on a network-ID
        // mismatch.
        let client_local = LocalNode::new_testnet(SecretKey::generate());
        let frames = responder_reply_frames(client_local, |hello| {
            // Wrong network id (compatible overlay version preserved).
            hello.network_id = stellar_xdr::Hash([0xAB; 32]);
        })
        .await;

        assert!(
            frames.len() >= 2,
            "responder must send HELLO then ERROR, got {} frame(s): {:?}",
            frames.len(),
            frames
                .iter()
                .map(helpers::message_type_name)
                .collect::<Vec<_>>()
        );
        assert!(
            matches!(frames[0], StellarMessage::Hello(_)),
            "first frame must be HELLO, got {}",
            helpers::message_type_name(&frames[0])
        );
        assert!(
            matches!(frames[1], StellarMessage::ErrorMsg(_)),
            "second frame must be ERROR_MSG, got {}",
            helpers::message_type_name(&frames[1])
        );
    }

    #[tokio::test]
    async fn test_responder_sends_hello_before_error_on_port_mismatch() {
        // Regression for #3067: a bad listening port is a peer-level check that
        // must also run AFTER the HELLO echo on the responder path.
        let client_local = LocalNode::new_testnet(SecretKey::generate());
        let frames = responder_reply_frames(client_local, |hello| {
            hello.listening_port = 0; // invalid
        })
        .await;

        assert!(
            matches!(frames.first(), Some(StellarMessage::Hello(_))),
            "first frame must be HELLO, got {:?}",
            frames
                .iter()
                .map(helpers::message_type_name)
                .collect::<Vec<_>>()
        );
        assert!(
            matches!(frames.get(1), Some(StellarMessage::ErrorMsg(_))),
            "second frame must be ERROR_MSG, got {:?}",
            frames
                .iter()
                .map(helpers::message_type_name)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_initiator_still_rejects_version_mismatch() {
        // Guard for #3067: the initiator path still runs the full phase-2
        // checks. We construct an initiator Peer and feed it a HELLO with an
        // incompatible overlay version; it must reject (drop) rather than
        // silently dropping the check after the responder reorder.
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let initiator_addr: SocketAddr = "127.0.0.1:11625".parse().unwrap();
        let remote_addr: SocketAddr = "127.0.0.1:11626".parse().unwrap();

        let initiator_conn =
            Connection::from_io(client_io, remote_addr, ConnectionDirection::Outbound).unwrap();
        let mut remote_conn =
            Connection::from_io(server_io, initiator_addr, ConnectionDirection::Inbound).unwrap();

        let initiator_local = LocalNode::new_testnet(SecretKey::generate());
        let initiator_auth = AuthContext::new(initiator_local, true);

        let mut initiator = Peer {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]),
                address: remote_addr,
                direction: ConnectionDirection::Outbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: Instant::now(),
                original_address: None,
            },
            state: PeerState::Connecting,
            connection: initiator_conn,
            auth: initiator_auth,
            stats: Arc::new(PeerStats::default()),
            metrics: Arc::new(OverlayMetrics::new()),
            holds_pending_peer_id: false,
            last_sent_msg_type: "none",
            recent_sends: VecDeque::with_capacity(RECENT_SENDS_CAPACITY),
        };

        // The remote side: a raw responder AuthContext that replies to the
        // initiator's HELLO with an incompatible-version HELLO of its own.
        let remote_local = LocalNode::new_testnet(SecretKey::generate());
        let remote_auth = AuthContext::new(remote_local, false);

        let task = tokio::spawn(async move { initiator.handshake(5, None, None, 0, 0).await });

        // Read the initiator's HELLO (so the duplex doesn't stall), then send
        // back a mismatched HELLO.
        let _ = remote_conn
            .recv_timeout(5)
            .await
            .expect("recv initiator hello");
        let mut bad_hello = remote_auth.create_hello();
        bad_hello.overlay_version = 5;
        bad_hello.overlay_min_version = 1;
        let frame = remote_auth.wrap_unauthenticated(StellarMessage::Hello(bad_hello));
        remote_conn.send(frame).await.expect("send bad hello");

        let res = task.await.expect("initiator task join");
        assert!(
            matches!(res, Err(OverlayError::VersionMismatch(_))),
            "initiator must reject incompatible overlay version, got {:?}",
            res
        );
    }

    // ---- #3773: per-peer outbound diagnostic ring (recent_sends) ----

    /// The ring records sends in FIFO order, newest last, and the summary
    /// reflects that order and count.
    #[tokio::test]
    async fn test_recent_sends_tracks_last_n_in_order() {
        let (mut peer_a, _peer_b) = Peer::new_test_authenticated_pair(
            Arc::new(OverlayMetrics::new()),
            Arc::new(OverlayMetrics::new()),
        );

        // Drive three post-auth sends of different message types.
        peer_a
            .send(StellarMessage::GetScpState(0))
            .await
            .expect("send 1");
        peer_a
            .send(StellarMessage::GetScpState(1))
            .await
            .expect("send 2");
        peer_a
            .send(StellarMessage::Peers(stellar_xdr::VecM::default()))
            .await
            .expect("send 3");

        assert_eq!(
            peer_a.recent_sends.len(),
            3,
            "three sends must produce three ring entries"
        );
        let types: Vec<&str> = peer_a.recent_sends.iter().map(|s| s.msg_type).collect();
        assert_eq!(
            types,
            vec!["GET_SCP_STATE", "GET_SCP_STATE", "PEERS"],
            "ring must preserve send order (oldest first)"
        );
        for entry in &peer_a.recent_sends {
            assert!(entry.wire_size > 0, "each entry must record a wire size");
        }
    }

    /// The ring is bounded at `RECENT_SENDS_CAPACITY`; older entries are evicted
    /// FIFO so the buffer never grows without bound.
    #[tokio::test]
    async fn test_recent_sends_bounded_at_capacity() {
        let (mut peer_a, _peer_b) = Peer::new_test_authenticated_pair(
            Arc::new(OverlayMetrics::new()),
            Arc::new(OverlayMetrics::new()),
        );

        // Send strictly more than the capacity so eviction is exercised. Each
        // send is wrapped with a strictly-incrementing authenticated sequence
        // number (`wrap_message`), so every recorded `prefix` is distinct even
        // though all messages share `msg_type == "GET_SCP_STATE"`. Capture each
        // just-recorded prefix so we can assert exactly which entries survive.
        let total = RECENT_SENDS_CAPACITY + 10;
        let mut sent_prefixes: Vec<[u8; crate::connection::SEND_PREFIX_LEN]> =
            Vec::with_capacity(total);
        for i in 0..total {
            peer_a
                .send(StellarMessage::GetScpState(i as u32))
                .await
                .expect("send");
            sent_prefixes.push(
                peer_a
                    .recent_sends
                    .back()
                    .expect("send just recorded an entry")
                    .prefix,
            );
        }

        assert_eq!(
            peer_a.recent_sends.len(),
            RECENT_SENDS_CAPACITY,
            "ring must be capped at RECENT_SENDS_CAPACITY entries"
        );

        // Prove FIFO front-eviction positionally: the surviving ring must be
        // exactly the tail slice of everything sent — the first
        // `total - RECENT_SENDS_CAPACITY` sends dropped from the front, with the
        // survivors retaining send order. This single equality proves (a) the
        // correct count was evicted, (b) from the front (not the back), and
        // (c) order preservation — none of which the length check alone can.
        let surviving: Vec<[u8; crate::connection::SEND_PREFIX_LEN]> =
            peer_a.recent_sends.iter().map(|s| s.prefix).collect();
        assert_eq!(
            surviving,
            sent_prefixes[total - RECENT_SENDS_CAPACITY..].to_vec(),
            "surviving ring must be the FIFO tail of all sends (front evicted)"
        );

        // Explicit front-eviction statement: the oldest survivor must NOT be the
        // last-evicted send. Distinct prefixes are guaranteed by the incrementing
        // auth sequence, so this is a strict inequality.
        assert_ne!(
            peer_a
                .recent_sends
                .front()
                .expect("ring is non-empty")
                .prefix,
            sent_prefixes[total - RECENT_SENDS_CAPACITY - 1],
            "front survivor must be the (total - CAPACITY)-th send, not the last evicted one"
        );
    }

    /// The human-readable summary lists the entry count and each entry's type,
    /// wire size, and byte prefix.
    #[tokio::test]
    async fn test_recent_sends_summary_format() {
        let (mut peer_a, _peer_b) = Peer::new_test_authenticated_pair(
            Arc::new(OverlayMetrics::new()),
            Arc::new(OverlayMetrics::new()),
        );

        assert_eq!(
            peer_a.recent_sends_summary(),
            "none",
            "an empty ring must summarize as \"none\""
        );

        peer_a
            .send(StellarMessage::GetScpState(7))
            .await
            .expect("send 1");
        peer_a
            .send(StellarMessage::Peers(stellar_xdr::VecM::default()))
            .await
            .expect("send 2");

        let summary = peer_a.recent_sends_summary();
        assert!(
            summary.starts_with("2 sends:"),
            "summary must lead with the entry count, got: {summary}"
        );
        assert!(
            summary.contains("GET_SCP_STATE"),
            "summary must name the message type, got: {summary}"
        );
        assert!(
            summary.contains("PEERS"),
            "summary must name every message type, got: {summary}"
        );
        assert!(
            summary.contains("size="),
            "summary must include the wire size, got: {summary}"
        );
        assert!(
            summary.contains("prefix="),
            "summary must include the byte prefix, got: {summary}"
        );
    }

    /// ALL four outbound send paths — `send`, `send_auth`, `send_raw`, and
    /// `send_batch` — must populate the ring with a real (non-zero) byte prefix
    /// that matches the actual encoded wire bytes. This is the core #3773
    /// diagnostic guarantee: whichever path carries the frame a peer rejects,
    /// we have captured its leading bytes.
    #[tokio::test]
    async fn test_recent_sends_captures_prefix_across_all_send_paths() {
        let (mut peer_a, _peer_b) = Peer::new_test_authenticated_pair(
            Arc::new(OverlayMetrics::new()),
            Arc::new(OverlayMetrics::new()),
        );

        // --- send_raw (unauthenticated framing; deterministic, so we can
        //     reproduce the exact captured prefix). ---
        let raw_msg = StellarMessage::Peers(stellar_xdr::VecM::default());
        peer_a.send_raw(raw_msg.clone()).await.expect("send_raw");
        let raw_entry = peer_a.recent_sends.back().expect("send_raw entry").clone();
        assert_eq!(raw_entry.msg_type, "PEERS");
        assert!(raw_entry.wire_size > 0);
        // Reproduce the exact wire bytes: unauthenticated framing is
        // deterministic (sequence 0, zero MAC), so this must match byte-for-byte.
        let reproduced = peer_a.auth.wrap_unauthenticated(raw_msg);
        let encoded = crate::codec::MessageCodec::encode_message(&reproduced).unwrap();
        let expected_prefix = crate::connection::encoded_body_prefix(&encoded);
        assert_eq!(
            raw_entry.prefix, expected_prefix,
            "send_raw prefix must match the actual encoded wire bytes"
        );

        // --- send_auth (authenticated Auth frame). ---
        peer_a
            .send_auth(StellarMessage::Auth(Auth {
                flags: AUTH_MSG_FLAG_FLOW_CONTROL_BYTES_REQUESTED,
            }))
            .await
            .expect("send_auth");
        let auth_entry = peer_a.recent_sends.back().expect("send_auth entry").clone();
        assert_eq!(auth_entry.msg_type, "AUTH");
        assert!(auth_entry.wire_size > 0);
        assert!(
            auth_entry.prefix.iter().any(|&b| b != 0),
            "send_auth must capture a non-zero byte prefix"
        );

        // --- send (post-auth flood/control frame). ---
        peer_a
            .send(StellarMessage::GetScpState(3))
            .await
            .expect("send");
        let send_entry = peer_a.recent_sends.back().expect("send entry").clone();
        assert_eq!(send_entry.msg_type, "GET_SCP_STATE");
        assert!(send_entry.wire_size > 0);
        assert!(
            send_entry.prefix.iter().any(|&b| b != 0),
            "send must capture a non-zero byte prefix"
        );

        // --- send_batch (coalesced write; prefix captured from its own local
        //     encode buffer, not from Connection::send). ---
        let before = peer_a.recent_sends.len();
        peer_a
            .send_batch(&[
                StellarMessage::GetScpState(4),
                StellarMessage::Peers(stellar_xdr::VecM::default()),
            ])
            .await
            .expect("send_batch");
        assert_eq!(
            peer_a.recent_sends.len(),
            before + 2,
            "send_batch must record one ring entry per batched message"
        );
        let batch_entries: Vec<_> = peer_a.recent_sends.iter().rev().take(2).cloned().collect();
        // rev().take(2) yields newest first: [GetPeers, GetScpState].
        assert_eq!(batch_entries[0].msg_type, "PEERS");
        assert_eq!(batch_entries[1].msg_type, "GET_SCP_STATE");
        for e in &batch_entries {
            assert!(e.wire_size > 0, "batch entry must record a wire size");
            assert!(
                e.prefix.iter().any(|&b| b != 0),
                "send_batch must capture a non-zero byte prefix for each message"
            );
        }
    }
}
