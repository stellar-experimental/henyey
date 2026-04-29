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
    flow_control::{msg_body_size, FlowControlConfig, INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES},
    LocalNode, OverlayError, PeerAddress, PeerId, Result,
};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use stellar_xdr::curr::{Auth, Hello, StellarMessage};
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
}

impl Peer {
    /// Connect to a peer and perform handshake.
    ///
    /// Uses the default initial byte grant (300KB). For production connections
    /// where `max_tx_size` may exceed the threshold, use
    /// [`connect_with_connection`] with a computed grant instead.
    #[allow(dead_code)]
    pub(crate) async fn connect(
        addr: &PeerAddress,
        local_node: LocalNode,
        timeouts: crate::OutboundTimeouts,
    ) -> Result<Self> {
        debug!("Connecting to peer: {}", addr);

        // Establish TCP connection
        let connection = Connection::connect(addr, timeouts.connect_secs).await?;

        // Create auth context (we called them)
        let auth = AuthContext::new(local_node, true);

        let mut peer = Self {
            info: PeerInfo {
                peer_id: PeerId::from_bytes([0u8; 32]), // Will be set after handshake
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
        };

        // Perform handshake
        peer.handshake(
            timeouts.auth_secs,
            None,
            None,
            INITIAL_PEER_FLOOD_READING_CAPACITY_BYTES,
        )
        .await?;

        Ok(peer)
    }

    /// Create an outbound peer from a pre-established transport connection.
    ///
    /// `initial_byte_grant` is the byte capacity sent in the initial
    /// SEND_MORE_EXTENDED — typically from [`FlowControlBytesConfig::bytes_total`].
    pub async fn connect_with_connection(
        addr: &PeerAddress,
        connection: Connection,
        local_node: LocalNode,
        auth_timeout_secs: u64,
        pending_peer_ids: Option<Arc<DashMap<PeerId, Instant>>>,
        initial_byte_grant: u32,
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
        };

        peer.handshake(
            auth_timeout_secs,
            None,
            pending_peer_ids,
            initial_byte_grant,
        )
        .await?;
        Ok(peer)
    }

    /// Create a peer from an accepted connection.
    ///
    /// `initial_byte_grant` is the byte capacity sent in the initial
    /// SEND_MORE_EXTENDED — typically from [`FlowControlBytesConfig::bytes_total`].
    pub async fn accept(
        connection: Connection,
        local_node: LocalNode,
        timeout_secs: u64,
        banned_peers: Arc<RwLock<HashSet<PeerId>>>,
        pending_peer_ids: Arc<DashMap<PeerId, Instant>>,
        initial_byte_grant: u32,
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
        };

        // Perform handshake (with ban + pending-dedup checks after HELLO for inbound)
        peer.handshake(
            timeout_secs,
            Some(banned_peers),
            Some(pending_peer_ids),
            initial_byte_grant,
        )
        .await?;

        Ok(peer)
    }

    /// Perform the authenticated handshake with a peer.
    ///
    /// OVERLAY_SPEC §4.2: The handshake ordering depends on direction:
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
        pending_peer_ids: Option<Arc<DashMap<PeerId, Instant>>>,
        initial_byte_grant: u32,
    ) -> Result<()> {
        self.state = PeerState::Handshaking;
        let handshake_start = std::time::Instant::now();
        debug!("Starting handshake with {}", self.connection.remote_addr());

        if self.connection.we_called_remote() {
            // --- Initiator (outbound): Send HELLO first, then receive ---
            self.send_hello().await?;
            self.recv_hello(auth_timeout_secs).await?;

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
                        e.insert(Instant::now());
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
                if let Some(ref pending) = pending_peer_ids {
                    pending.remove(&self.info.peer_id);
                }
                return Err(e);
            }
        } else {
            // --- Responder (inbound): Receive HELLO first, then reply ---
            self.recv_hello(auth_timeout_secs).await?;

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

            // Reserve a pending peer-ID slot immediately after HELLO reveals
            // the remote identity. Mirrors stellar-core Peer::recvHello()
            // which rejects duplicates against getPendingPeers() before AUTH.
            // Without this, N concurrent connections from the same peer_id
            // can all occupy pending slots until AUTH completes.
            if let Some(ref pending) = pending_peer_ids {
                use dashmap::mapref::entry::Entry;
                match pending.entry(self.info.peer_id.clone()) {
                    Entry::Occupied(_) => {
                        warn!(
                            "Rejected duplicate inbound peer {} — handshake already in flight",
                            self.info.peer_id
                        );
                        return Err(OverlayError::PeerDuplicate(self.info.peer_id.to_string()));
                    }
                    Entry::Vacant(e) => {
                        e.insert(Instant::now());
                    }
                }
            }

            // Remaining handshake steps after peer_id reservation.
            // If any step fails, clean up the pending peer_id reservation.
            let result: Result<()> = async {
                self.send_hello().await?;
                self.recv_auth(auth_timeout_secs).await?;
                self.send_auth_msg().await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                if let Some(ref pending) = pending_peer_ids {
                    pending.remove(&self.info.peer_id);
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
        // The byte grant is computed from max_tx_size by the caller via
        // FlowControlBytesConfig::bytes_total() — must match the FlowControl
        // initial capacity for this peer.
        let send_more = StellarMessage::SendMoreExtended(stellar_xdr::curr::SendMoreExtended {
            num_messages: FlowControlConfig::default().peer_flood_reading_capacity as u32,
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

    /// Receive and process the peer's HELLO message.
    async fn recv_hello(&mut self, timeout_secs: u64) -> Result<()> {
        let frame = self
            .connection
            .recv_timeout(timeout_secs)
            .await?
            .ok_or_else(|| OverlayError::PeerDisconnected("no Hello received".to_string()))?;
        debug!("Received frame with {} bytes", frame.raw_len);

        let message = self.auth.unwrap_message(frame.message)?;

        match message {
            StellarMessage::Hello(peer_hello) => {
                self.process_hello(peer_hello)?;
            }
            other => {
                return Err(OverlayError::InvalidMessage(format!(
                    "expected Hello, got {}",
                    helpers::message_type_name(&other)
                )));
            }
        }
        Ok(())
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
        let frame = self
            .connection
            .recv_timeout(timeout_secs)
            .await?
            .ok_or_else(|| OverlayError::PeerDisconnected("no Auth received".to_string()))?;

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
                let err_msg: String = err.msg.to_string();
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

    /// Process a received Hello message.
    fn process_hello(&mut self, hello: Hello) -> Result<()> {
        // State guard: reject if not in Handshaking state
        if self.state != PeerState::Handshaking {
            return Err(OverlayError::InvalidMessage(format!(
                "received Hello in unexpected state {:?}",
                self.state
            )));
        }

        // Port validation: XDR uses i32, but valid ports are 1-65535.
        // Reject port 0 — matches stellar-core Peer::recvHello() which rejects
        // listeningPort <= 0 to prevent poisoning peer gossip with ephemeral ports.
        if hello.listening_port <= 0 || hello.listening_port > u16::MAX as i32 {
            return Err(OverlayError::InvalidMessage(format!(
                "invalid listening port: {}",
                hello.listening_port
            )));
        }

        // Let auth context process it (network ID, version, cert checks)
        self.auth.process_hello(&hello)?;

        // Extract peer info
        let peer_id = self
            .auth
            .peer_id()
            .cloned()
            .ok_or_else(|| OverlayError::AuthenticationFailed("no peer ID".to_string()))?;

        // Self-connection check: reject if peer is ourselves
        let local_peer_id = self.auth.local_peer_id();
        if peer_id == local_peer_id {
            return Err(OverlayError::InvalidMessage(
                "received Hello from self".to_string(),
            ));
        }

        let version_string: String = hello.version_str.to_string();

        self.info.peer_id = peer_id;
        self.info.version_string = version_string;
        self.info.overlay_version = hello.overlay_version;
        self.info.ledger_version = hello.ledger_version;
        if hello.listening_port > 0 {
            let port = hello.listening_port as u16;
            let ip = self.info.address.ip();
            self.info.address = SocketAddr::new(ip, port);
        }

        debug!(
            "Received Hello from {} (version: {}, overlay: {})",
            self.info.peer_id, self.info.version_string, self.info.overlay_version
        );

        Ok(())
    }

    /// Send a raw message (before authentication, e.g., Hello).
    async fn send_raw(&mut self, message: StellarMessage) -> Result<()> {
        let size = msg_body_size(&message);
        let auth_msg = self.auth.wrap_unauthenticated(message);
        self.connection.send(auth_msg).await?;
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(size, Ordering::Relaxed);
        Ok(())
    }

    /// Send an Auth message (with MAC but sequence 0).
    async fn send_auth(&mut self, message: StellarMessage) -> Result<()> {
        let size = msg_body_size(&message);
        let auth_msg = self.auth.wrap_auth_message(message)?;
        self.connection.send(auth_msg).await?;
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(size, Ordering::Relaxed);
        Ok(())
    }

    /// Send a message to this peer.
    pub async fn send(&mut self, message: StellarMessage) -> Result<()> {
        if self.state != PeerState::Authenticated {
            return Err(OverlayError::PeerDisconnected(
                "not authenticated".to_string(),
            ));
        }

        let msg_type = helpers::message_type_name(&message);
        trace!("SEND {} to {}", msg_type, self.info.peer_id);

        let size = msg_body_size(&message);
        let auth_msg = self.auth.wrap_message(message)?;
        self.connection.send(auth_msg).await?;
        self.stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(size, Ordering::Relaxed);

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

    /// Request SCP state from peer.
    pub async fn request_scp_state(&mut self, ledger_seq: u32) -> Result<()> {
        let message = StellarMessage::GetScpState(ledger_seq);
        self.send(message).await
    }

    /// Request peers from this peer.
    /// Note: GetPeers was removed in Protocol 24. This is a no-op.
    /// Send flow control message.
    pub async fn send_more(&mut self, num_messages: u32) -> Result<()> {
        let message = StellarMessage::SendMore(stellar_xdr::curr::SendMore { num_messages });
        self.send(message).await
    }

    /// Send extended flow control message with byte limit.
    pub async fn send_more_extended(&mut self, num_messages: u32, num_bytes: u32) -> Result<()> {
        let message = StellarMessage::SendMoreExtended(stellar_xdr::curr::SendMoreExtended {
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
}
