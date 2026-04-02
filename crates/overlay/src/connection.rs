//! Low-level TCP connection handling for the Stellar overlay.
//!
//! This module provides the transport layer for overlay connections:
//!
//! - [`Connection`] - A single TCP connection with framed message I/O
//! - [`Listener`] - Accepts incoming TCP connections on a port
//! - [`ConnectionPool`] - Tracks connection counts against limits
//!
//! # Architecture
//!
//! Connections wrap a TCP stream with the [`MessageCodec`] for automatic
//! length-prefixed framing. The higher-level [`Peer`] type handles
//! authentication and message processing on top of a `Connection`.
//!
//! [`MessageCodec`]: crate::MessageCodec
//! [`Peer`]: crate::Peer

use crate::{
    codec::{MessageCodec, MessageFrame},
    OverlayError, PeerAddress, Result,
};
use futures::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use stellar_xdr::curr::AuthenticatedMessage;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_util::codec::Framed;
use tracing::{debug, trace};

trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

type BoxedIo = Pin<Box<dyn AsyncIo>>;

/// Direction of a peer connection.
///
/// Used to determine the initiator/acceptor roles during key derivation
/// in the authentication handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionDirection {
    /// We initiated the connection (we are the "initiator" in key derivation).
    Outbound,
    /// The peer connected to us (we are the "acceptor" in key derivation).
    Inbound,
}

impl ConnectionDirection {
    /// Returns true if we initiated this connection.
    ///
    /// This determines our role in the authentication key derivation.
    pub fn we_called_remote(&self) -> bool {
        matches!(self, ConnectionDirection::Outbound)
    }
}

/// A TCP connection to a peer with framed message I/O.
///
/// Wraps a TCP stream with the [`MessageCodec`] for automatic message framing.
/// Provides async methods for sending and receiving [`AuthenticatedMessage`]s.
///
/// # Lifecycle
///
/// 1. Create via [`Connection::connect`] (outbound) or from [`Listener::accept`] (inbound)
/// 2. Use [`send`](Connection::send) and [`recv`](Connection::recv) for message I/O
/// 3. Call [`close`](Connection::close) when done (or drop the connection)
///
/// [`MessageCodec`]: crate::MessageCodec
/// [`AuthenticatedMessage`]: stellar_xdr::curr::AuthenticatedMessage
pub struct Connection {
    /// Framed stream for message encoding/decoding.
    framed: Framed<BoxedIo, MessageCodec>,
    /// Remote peer's socket address.
    remote_addr: SocketAddr,
    /// Whether we initiated or accepted this connection.
    direction: ConnectionDirection,
    /// True if the connection has been closed.
    closed: bool,
}

impl Connection {
    /// Creates a connection from an existing TCP stream.
    ///
    /// Configures TCP_NODELAY to reduce latency and wraps the stream
    /// with the message codec.
    pub fn new(stream: TcpStream, direction: ConnectionDirection) -> Result<Self> {
        let remote_addr = stream.peer_addr()?;

        // Spec: OVERLAY_SPEC §4.1 — TCP_NODELAY and SO_LINGER MUST be set.
        stream.set_nodelay(true)?;
        // SO_LINGER with timeout=0 causes immediate close (RST) without waiting.
        #[allow(deprecated)]
        stream.set_linger(Some(Duration::from_secs(0)))?;

        Self::from_boxed_io(Box::pin(stream), remote_addr, direction)
    }

    /// Creates a connection from an arbitrary async IO stream.
    pub fn from_io<T>(
        stream: T,
        remote_addr: SocketAddr,
        direction: ConnectionDirection,
    ) -> Result<Self>
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        Self::from_boxed_io(Box::pin(stream), remote_addr, direction)
    }

    fn from_boxed_io(
        stream: BoxedIo,
        remote_addr: SocketAddr,
        direction: ConnectionDirection,
    ) -> Result<Self> {
        let framed = Framed::new(stream, MessageCodec::new());

        Ok(Self {
            framed,
            remote_addr,
            direction,
            closed: false,
        })
    }

    /// Connects to a peer address with a timeout.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionTimeout` if the connection is not established
    /// within `timeout_secs`, or `ConnectionFailed` for other errors.
    pub async fn connect(addr: &PeerAddress, timeout_secs: u64) -> Result<Self> {
        debug!("Connecting to peer: {}", addr);

        let socket_addr = addr.to_socket_addr();
        let connect_timeout = Duration::from_secs(timeout_secs);

        let stream = timeout(connect_timeout, TcpStream::connect(&socket_addr))
            .await
            .map_err(|_| OverlayError::ConnectionTimeout(addr.to_string()))?
            .map_err(|e| OverlayError::ConnectionFailed(format!("{}: {}", addr, e)))?;

        debug!("Connected to peer: {}", addr);
        Self::new(stream, ConnectionDirection::Outbound)
    }

    /// Returns the remote peer's socket address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Returns whether this is an inbound or outbound connection.
    pub fn direction(&self) -> ConnectionDirection {
        self.direction
    }

    /// Returns true if we initiated this connection.
    pub fn we_called_remote(&self) -> bool {
        self.direction.we_called_remote()
    }

    /// Returns true if the connection has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Marks the codec as authenticated, allowing full-size messages.
    ///
    /// Must be called after the handshake completes. Before this,
    /// incoming messages are limited to 4,096 bytes.
    pub fn set_authenticated(&mut self) {
        self.framed.codec_mut().set_authenticated();
    }

    /// Sends an authenticated message to the peer.
    ///
    /// # Errors
    ///
    /// Returns `PeerDisconnected` if the connection is already closed,
    /// or a codec error if encoding fails.
    pub async fn send(&mut self, message: AuthenticatedMessage) -> Result<()> {
        if self.closed {
            return Err(OverlayError::PeerDisconnected(
                "connection closed".to_string(),
            ));
        }

        trace!("Sending message to {}", self.remote_addr);

        // Add timeout to prevent blocking indefinitely on TCP backpressure
        const SEND_TIMEOUT_SECS: u64 = 10;
        match timeout(
            Duration::from_secs(SEND_TIMEOUT_SECS),
            self.framed.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.closed = true;
                Err(e)
            }
            Err(_) => {
                self.closed = true;
                Err(OverlayError::ConnectionTimeout(format!(
                    "send timeout after {}s to {}",
                    SEND_TIMEOUT_SECS, self.remote_addr
                )))
            }
        }
    }

    /// Receives the next message from the peer.
    ///
    /// Returns `Ok(None)` if the connection was closed cleanly by the peer.
    pub async fn recv(&mut self) -> Result<Option<MessageFrame>> {
        if self.closed {
            debug!("recv called but connection already closed");
            return Ok(None);
        }

        match self.framed.next().await {
            Some(Ok(frame)) => {
                debug!(
                    "Received message from {} ({} bytes)",
                    self.remote_addr, frame.raw_len
                );
                Ok(Some(frame))
            }
            Some(Err(e)) => {
                debug!("Error receiving from {}: {}", self.remote_addr, e);
                self.closed = true;
                Err(e)
            }
            None => {
                debug!("Connection closed by peer: {}", self.remote_addr);
                self.closed = true;
                Ok(None)
            }
        }
    }

    /// Receives a message with a timeout.
    ///
    /// # Errors
    ///
    /// Returns `ConnectionTimeout` if no message is received within `timeout_secs`.
    pub async fn recv_timeout(&mut self, timeout_secs: u64) -> Result<Option<MessageFrame>> {
        let recv_timeout = Duration::from_secs(timeout_secs);

        match timeout(recv_timeout, self.recv()).await {
            Ok(result) => result,
            Err(_) => {
                self.closed = true;
                Err(OverlayError::ConnectionTimeout(
                    "receive timeout".to_string(),
                ))
            }
        }
    }

    /// Closes the connection.
    ///
    /// This marks the connection as closed. The underlying TCP stream
    /// will be closed when the connection is dropped.
    pub async fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            debug!("Closed connection to {}", self.remote_addr);
        }
    }

    /// Splits the connection into separate send and receive halves.
    ///
    /// This allows concurrent sending and receiving on the same connection.
    pub fn split(self) -> (ConnectionSender, ConnectionReceiver) {
        let (sink, stream) = self.framed.split();
        (
            ConnectionSender {
                sink,
                remote_addr: self.remote_addr,
            },
            ConnectionReceiver {
                stream,
                remote_addr: self.remote_addr,
            },
        )
    }
}

/// Send half of a split connection.
///
/// Created by [`Connection::split`]. Allows sending messages without
/// holding a lock on the full connection.
pub struct ConnectionSender {
    sink: futures::stream::SplitSink<Framed<BoxedIo, MessageCodec>, AuthenticatedMessage>,
    remote_addr: SocketAddr,
}

impl ConnectionSender {
    /// Sends a message to the peer.
    ///
    /// Includes a timeout to prevent blocking indefinitely on TCP backpressure,
    /// matching the timeout behavior of [`Connection::send`].
    pub async fn send(&mut self, message: AuthenticatedMessage) -> Result<()> {
        trace!("Sending message to {}", self.remote_addr);
        const SEND_TIMEOUT_SECS: u64 = 10;
        match timeout(
            Duration::from_secs(SEND_TIMEOUT_SECS),
            self.sink.send(message),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(OverlayError::ConnectionTimeout(format!(
                "send timeout after {}s to {}",
                SEND_TIMEOUT_SECS, self.remote_addr
            ))),
        }
    }

    /// Returns the remote peer's socket address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

/// Receive half of a split connection.
///
/// Created by [`Connection::split`]. Allows receiving messages without
/// holding a lock on the full connection.
pub struct ConnectionReceiver {
    stream: futures::stream::SplitStream<Framed<BoxedIo, MessageCodec>>,
    remote_addr: SocketAddr,
}

impl ConnectionReceiver {
    /// Receives the next message from the peer.
    ///
    /// Returns `Ok(None)` if the connection was closed.
    pub async fn recv(&mut self) -> Result<Option<MessageFrame>> {
        match self.stream.next().await {
            Some(Ok(frame)) => {
                trace!(
                    "Received message from {} ({} bytes)",
                    self.remote_addr,
                    frame.raw_len
                );
                Ok(Some(frame))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Returns the remote peer's socket address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

/// TCP listener for accepting incoming peer connections.
///
/// Binds to a port and accepts new connections, wrapping them as
/// inbound [`Connection`]s.
pub struct Listener {
    listener: ListenerKind,
    local_addr: SocketAddr,
}

enum ListenerKind {
    Tcp(TcpListener),
    Loopback(tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Connection>>),
}

impl Listener {
    /// Binds to the specified port on all interfaces (0.0.0.0).
    ///
    /// # Errors
    ///
    /// Returns an IO error if the port is already in use or binding fails.
    pub async fn bind(port: u16) -> Result<Self> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).await?;
        let local_addr = listener.local_addr()?;

        debug!("Listening on {}", local_addr);

        Ok(Self {
            listener: ListenerKind::Tcp(listener),
            local_addr,
        })
    }

    pub fn from_loopback(
        local_addr: SocketAddr,
        receiver: tokio::sync::mpsc::Receiver<Connection>,
    ) -> Self {
        Self {
            listener: ListenerKind::Loopback(tokio::sync::Mutex::new(receiver)),
            local_addr,
        }
    }

    /// Returns the local address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Accepts the next incoming connection.
    ///
    /// Blocks until a new connection arrives, then returns it as an
    /// inbound [`Connection`].
    pub async fn accept(&self) -> Result<Connection> {
        match &self.listener {
            ListenerKind::Tcp(listener) => {
                let (stream, remote_addr) = listener.accept().await?;
                debug!("Accepted connection from {}", remote_addr);
                Connection::new(stream, ConnectionDirection::Inbound)
            }
            ListenerKind::Loopback(receiver) => {
                receiver.lock().await.recv().await.ok_or_else(|| {
                    OverlayError::PeerDisconnected("loopback listener closed".to_string())
                })
            }
        }
    }
}

/// Thread-safe connection counter for enforcing connection limits.
///
/// Used to track inbound and outbound connection counts separately,
/// ensuring we don't exceed configured limits.
///
/// Tracks both pending (unauthenticated) and authenticated connections
/// separately, matching stellar-core's `OverlayManagerImpl` which
/// distinguishes `mPendingPeers` from `mAuthenticatedPeers`.
///
/// For inbound pools, supports "possibly preferred" extra slots:
/// connections from preferred IP addresses can exceed `max_connections`
/// by up to `possibly_preferred_extra` slots, matching upstream's
/// `Config::POSSIBLY_PREFERRED_EXTRA`.
pub struct ConnectionPool {
    /// Maximum number of authenticated connections allowed.
    max_connections: usize,
    /// Maximum additional pending (handshaking) connections allowed beyond
    /// `max_connections`. This allows incoming peers to complete the handshake
    /// and receive PEERS even when all authenticated slots are full, matching
    /// stellar-core's behavior of always sending PEERS before ERR_LOAD.
    max_pending_extra: usize,
    /// Extra slots for connections from preferred IP addresses.
    possibly_preferred_extra: usize,
    /// Set of preferred IP addresses that qualify for extra slots.
    preferred_ips: parking_lot::RwLock<HashSet<IpAddr>>,
    /// Current number of active connections (pending + authenticated).
    current_count: AtomicUsize,
    /// Number of pending (unauthenticated) connections.
    /// A connection starts as pending and transitions to authenticated
    /// after the Hello/Auth handshake completes.
    pending_count: AtomicUsize,
    /// Number of authenticated connections.
    authenticated_count: AtomicUsize,
}

/// Default extra pending slots beyond `max_connections`.
/// Allows incoming connections to complete handshake and receive PEERS
/// even when all authenticated slots are full.
const DEFAULT_MAX_PENDING_EXTRA: usize = 32;

impl ConnectionPool {
    /// Creates a new connection pool with the given limit.
    pub fn new(max_connections: usize) -> Self {
        Self {
            max_connections,
            max_pending_extra: DEFAULT_MAX_PENDING_EXTRA,
            possibly_preferred_extra: 0,
            preferred_ips: parking_lot::RwLock::new(HashSet::new()),
            current_count: AtomicUsize::new(0),
            pending_count: AtomicUsize::new(0),
            authenticated_count: AtomicUsize::new(0),
        }
    }

    /// Creates a new connection pool with extra slots for preferred peers.
    pub fn with_preferred(
        max_connections: usize,
        possibly_preferred_extra: usize,
        preferred_ips: HashSet<IpAddr>,
    ) -> Self {
        Self {
            max_connections,
            max_pending_extra: DEFAULT_MAX_PENDING_EXTRA,
            possibly_preferred_extra,
            preferred_ips: parking_lot::RwLock::new(preferred_ips),
            current_count: AtomicUsize::new(0),
            pending_count: AtomicUsize::new(0),
            authenticated_count: AtomicUsize::new(0),
        }
    }

    /// Returns true if there's room for another connection (including pending headroom).
    pub fn can_accept(&self) -> bool {
        self.current_count.load(Ordering::Relaxed) < self.max_connections + self.max_pending_extra
    }

    /// Attempts to reserve a connection slot.
    ///
    /// Returns true if a slot was reserved, false if the limit is reached.
    /// The reserved slot starts as "pending" (unauthenticated).
    /// Uses atomic compare-and-swap for thread safety.
    pub fn try_reserve(&self) -> bool {
        self.try_reserve_with_ip(None)
    }

    /// Attempts to reserve a connection slot, with optional preferred IP check.
    ///
    /// The total connection limit is `max_connections + max_pending_extra`,
    /// allowing incoming peers to complete handshake and receive PEERS even
    /// when all authenticated slots are full. Preferred IPs get additional
    /// slots via `possibly_preferred_extra`.
    ///
    /// The reserved slot starts as "pending" (unauthenticated). After
    /// handshake and PEERS exchange, call [`try_promote_to_authenticated`]
    /// to check the authenticated limit.
    pub fn try_reserve_with_ip(&self, ip: Option<IpAddr>) -> bool {
        let mut current = self.current_count.load(Ordering::Relaxed);
        loop {
            // Base limit: authenticated max + pending headroom
            let base_max = self.max_connections + self.max_pending_extra;
            let effective_max = if current >= base_max {
                // Over the base limit -- only allow if IP is preferred
                if let Some(ref addr) = ip {
                    let preferred = self.preferred_ips.read();
                    if preferred.contains(addr) {
                        base_max + self.possibly_preferred_extra
                    } else {
                        base_max
                    }
                } else {
                    base_max
                }
            } else {
                base_max
            };

            if current >= effective_max {
                return false;
            }
            match self.current_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.pending_count.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Err(new_current) => current = new_current,
            }
        }
    }

    /// Releases a previously reserved connection slot.
    ///
    /// Call this when a connection is closed. If the connection was still
    /// pending (never promoted to authenticated), the pending count is
    /// also decremented.
    pub fn release(&self) {
        self.current_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Releases a pending connection slot.
    ///
    /// Call this when a pending (unauthenticated) connection fails or is
    /// rejected before completing the handshake.
    pub fn release_pending(&self) {
        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        self.current_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Releases an authenticated connection slot.
    ///
    /// Call this when an authenticated peer disconnects.
    pub fn release_authenticated(&self) {
        self.authenticated_count.fetch_sub(1, Ordering::Relaxed);
        self.current_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Marks a connection as authenticated (promotes from pending).
    ///
    /// Call this after the Hello/Auth handshake completes successfully.
    pub fn mark_authenticated(&self) {
        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        self.authenticated_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Try to promote a pending connection to authenticated.
    ///
    /// Returns true if authenticated_count < max_connections and the promotion
    /// succeeded. Returns false if all authenticated slots are full (the
    /// connection remains pending and the caller should reject with ERR_LOAD).
    ///
    /// Matches stellar-core `acceptAuthenticatedPeer`
    /// (OverlayManagerImpl.cpp:206): PEERS are already sent by this point,
    /// so crawlers get topology data even when rejected.
    pub fn try_promote_to_authenticated(&self) -> bool {
        let authenticated = self.authenticated_count.load(Ordering::Relaxed);
        if authenticated >= self.max_connections {
            return false;
        }
        self.mark_authenticated();
        true
    }

    /// Returns the current number of connections (pending + authenticated).
    pub fn count(&self) -> usize {
        self.current_count.load(Ordering::Relaxed)
    }

    /// Returns the number of pending (unauthenticated) connections.
    pub fn pending_count(&self) -> usize {
        self.pending_count.load(Ordering::Relaxed)
    }

    /// Returns the number of authenticated connections.
    pub fn authenticated_count(&self) -> usize {
        self.authenticated_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_direction() {
        assert!(ConnectionDirection::Outbound.we_called_remote());
        assert!(!ConnectionDirection::Inbound.we_called_remote());
    }

    #[test]
    fn test_connection_pool() {
        let pool = ConnectionPool::new(2);
        assert!(pool.can_accept());
        assert_eq!(pool.count(), 0);
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.authenticated_count(), 0);

        assert!(pool.try_reserve());
        assert_eq!(pool.count(), 1);
        assert_eq!(pool.pending_count(), 1);

        assert!(pool.try_reserve());
        assert_eq!(pool.count(), 2);
        assert_eq!(pool.pending_count(), 2);

        // Can still reserve more (pending headroom allows handshake + PEERS)
        assert!(pool.try_reserve());
        assert_eq!(pool.count(), 3);

        // Promote one to authenticated
        pool.mark_authenticated();
        assert_eq!(pool.pending_count(), 2);
        assert_eq!(pool.authenticated_count(), 1);
        assert_eq!(pool.count(), 3); // total unchanged

        // try_promote_to_authenticated succeeds (1 < max_connections=2)
        assert!(pool.try_promote_to_authenticated());
        assert_eq!(pool.authenticated_count(), 2);
        assert_eq!(pool.pending_count(), 1); // one pending remains

        // try_promote_to_authenticated fails (2 >= max_connections=2)
        assert!(!pool.try_promote_to_authenticated());

        // Release the remaining pending one
        pool.release_pending();
        assert_eq!(pool.count(), 2);
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.authenticated_count(), 2);

        // Release an authenticated one
        pool.release_authenticated();
        assert!(pool.can_accept());
        assert_eq!(pool.count(), 1);
        assert_eq!(pool.authenticated_count(), 1);

        // Release the last one
        pool.release_authenticated();
        assert_eq!(pool.count(), 0);
        assert_eq!(pool.pending_count(), 0);
        assert_eq!(pool.authenticated_count(), 0);
    }

    #[test]
    fn test_connection_pool_preferred_ip() {
        // Use max_pending_extra=0 to test preferred IP behavior in isolation
        let preferred: std::collections::HashSet<std::net::IpAddr> =
            vec!["10.0.0.1".parse().unwrap()].into_iter().collect();
        let mut pool = ConnectionPool::with_preferred(2, 2, preferred);
        pool.max_pending_extra = 0; // disable pending headroom for this test

        // Fill to base limit
        assert!(pool.try_reserve()); // 1
        assert!(pool.try_reserve()); // 2

        // Non-preferred IP is rejected at base limit
        let non_preferred: std::net::IpAddr = "10.0.0.2".parse().unwrap();
        assert!(!pool.try_reserve_with_ip(Some(non_preferred)));

        // Preferred IP is allowed in extra slots
        let preferred_ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        assert!(pool.try_reserve_with_ip(Some(preferred_ip))); // 3
        assert!(pool.try_reserve_with_ip(Some(preferred_ip))); // 4

        // Even preferred IP is rejected when extra slots are full
        assert!(!pool.try_reserve_with_ip(Some(preferred_ip)));
        assert_eq!(pool.count(), 4);

        // Release one and preferred can get back in
        pool.release();
        assert!(pool.try_reserve_with_ip(Some(preferred_ip)));
    }

    #[test]
    fn test_connection_pool_no_ip_at_limit() {
        let mut pool = ConnectionPool::new(2);
        pool.max_pending_extra = 0; // disable pending headroom for this test
        assert!(pool.try_reserve());
        assert!(pool.try_reserve());

        // try_reserve_with_ip(None) should fail at limit when no pending headroom
        assert!(!pool.try_reserve_with_ip(None));
    }

    #[test]
    fn test_connection_pool_pending_headroom() {
        // With max_connections=2 and default pending extra (32),
        // we can accept many pending connections beyond the auth limit.
        let pool = ConnectionPool::new(2);

        // Fill authenticated slots
        assert!(pool.try_reserve());
        pool.mark_authenticated();
        assert!(pool.try_reserve());
        pool.mark_authenticated();
        assert_eq!(pool.authenticated_count(), 2);

        // We can still accept pending connections (for handshake + PEERS)
        assert!(pool.try_reserve());
        assert_eq!(pool.pending_count(), 1);

        // But promotion fails since authenticated is at max
        assert!(!pool.try_promote_to_authenticated());
        assert_eq!(pool.authenticated_count(), 2);

        // Release the pending one (simulates sending ERR_LOAD + close)
        pool.release_pending();
        assert_eq!(pool.count(), 2);
    }

    #[tokio::test]
    async fn test_peer_address_connect_format() {
        let addr = PeerAddress::new("127.0.0.1", 11625);
        assert_eq!(addr.to_socket_addr(), "127.0.0.1:11625");
    }
}
