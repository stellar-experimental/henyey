//! P2P networking for henyey.
//!
//! This crate implements the Stellar overlay network protocol, enabling nodes to
//! communicate with each other for consensus, transaction propagation, and state
//! synchronization. It provides:
//!
//! - **Peer discovery and connection management** - Automatic connection to known peers
//!   with support for preferred peers and connection limits
//! - **Authenticated peer connections** - X25519 key exchange with HMAC-SHA256 message
//!   authentication following the Stellar overlay protocol
//! - **Message routing and flooding** - Intelligent message propagation with duplicate
//!   detection and rate limiting
//! - **Flow control** - Bandwidth management using SendMore/SendMoreExtended messages
//!
//! # Architecture
//!
//! The crate is organized around these key components:
//!
//! - [`OverlayManager`] - Central coordinator that manages all peer connections,
//!   handles message routing, and provides the main API for the overlay network
//! - [`Peer`] - Represents an authenticated connection to a single peer
//! - [`FloodGate`] - Tracks seen messages to prevent duplicate flooding
//! - [`AuthContext`] - Manages the authentication handshake and message MAC verification
//!
//! # Protocol Overview
//!
//! The overlay uses TCP connections with length-prefixed XDR-encoded messages.
//! Each connection begins with an authentication handshake:
//!
//! 1. Both peers exchange `Hello` messages containing their public key and auth certificate
//! 2. X25519 key exchange derives shared secrets for message authentication
//! 3. Both peers send `Auth` messages to complete the handshake
//! 4. All subsequent messages are authenticated with HMAC-SHA256
//!
//! # Message Types
//!
//! The overlay handles various message types defined in the Stellar XDR:
//!
//! - **Hello/Auth** - Initial handshake establishing authenticated channel
//! - **Peers** - Peer address exchange for network discovery
//! - **Transaction** - Transaction broadcasting and flooding
//! - **SCP** - Stellar Consensus Protocol messages
//! - **GetScpState** - Request peer's current SCP state
//! - **TxSet/GetTxSet** - Transaction set exchange
//! - **FloodAdvert/FloodDemand** - Pull-based transaction flooding
//! - **SendMore/SendMoreExtended** - Flow control messages
//!
//! # Example
//!
//! ```rust,no_run
//! use henyey_overlay::{OverlayConfig, OverlayManager, LocalNode};
//! use henyey_crypto::SecretKey;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Generate a node identity
//! let secret_key = SecretKey::generate();
//! let local_node = LocalNode::new_testnet(secret_key);
//!
//! // Create overlay manager with testnet configuration
//! let config = OverlayConfig::testnet();
//! let mut manager = OverlayManager::new(config, local_node)?;
//!
//! // Start the overlay (connects to known peers)
//! manager.start().await?;
//!
//! // Subscribe to incoming messages
//! let mut rx = manager.subscribe();
//! while let Ok(msg) = rx.recv().await {
//!     println!("Received message from {}", msg.from_peer);
//! }
//! # Ok(())
//! # }
//! ```

mod auth;
mod ban_manager;
mod codec;
mod connection;
mod connection_factory;
mod error;
mod flood;
mod flow_control;
mod item_fetcher;
mod loopback;
mod manager;
mod message_handlers;
mod metrics;
mod peer;
mod peer_manager;
pub mod query_policy;
mod survey;
mod tx_adverts;
mod tx_demands;

// Re-export public types
pub use auth::{AuthCert, AuthCertExt, AuthContext, AuthState};
pub use ban_manager::BanManager;
pub use codec::{helpers as message_helpers, MessageCodec, MessageFrame};
pub use connection::{Connection, ConnectionDirection, ConnectionPool, Listener};
pub use connection_factory::{ConnectionFactory, TcpConnectionFactory};
pub use error::OverlayError;
pub use flood::{compute_message_hash, FloodGate, FloodGateStats, FloodRecord};
pub use flow_control::{
    is_flow_controlled_message, FlowControl, FlowControlBytesConfig, FlowControlConfig,
    FlowControlStats, MessagePriority, QueuedOutboundMessage, ScpQueueCallback, SendMoreCapacity,
};
pub use item_fetcher::{
    ItemFetcher, ItemFetcherConfig, ItemFetcherStats, ItemType, NextPeerResult, PendingRequest,
    Tracker,
};
pub use loopback::LoopbackConnectionFactory;
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub use manager::TestPeerReceiver;
pub use manager::{OverlayManager, OverlayMessage, OverlayStats, PeerSnapshot};
pub use message_handlers::{MessageDispatcher, MessageDispatcherStats, TxSetData};
pub use metrics::{Counter, OverlayMetrics, OverlayMetricsSnapshot, Timer, TimerSnapshot};
pub use peer::{Peer, PeerInfo, PeerState, PeerStats, PeerStatsSnapshot};
pub use peer_manager::{
    BackOffUpdate, PeerManager, PeerQuery, PeerRecord, PeerTypeFilter, StoredPeerType, TypeUpdate,
};
pub use survey::{
    CollectingNodeData, CollectingPeerData, SurveyConfig, SurveyManager, SurveyManagerStats,
    SurveyPhase, TimeSlicedNodeData, TimeSlicedPeerData, SURVEY_THROTTLE_TIMEOUT_MULT,
};
pub use tx_adverts::{TxAdverts, TxAdvertsConfig, TxAdvertsStats, TX_ADVERT_VECTOR_MAX_SIZE};
pub use tx_demands::{
    CleanupResult, DemandStatus, PeerDemandResult, TxDemandsConfig, TxDemandsManager,
    TxDemandsStats, TxKnownStatus, TxPullLatency, MAX_RETRY_COUNT, TX_DEMAND_VECTOR_MAX_SIZE,
};

use std::collections::HashSet;
use tokio::sync::mpsc;

/// Result type for overlay operations.
pub type Result<T> = std::result::Result<T, OverlayError>;

/// Timeouts for outbound peer connections.
///
/// Separates the TCP connect phase (governed by network latency) from the
/// post-connect authentication handshake. The `auth_secs` field governs each
/// individual unauthenticated receive gap (recv_hello, recv_auth), matching
/// stellar-core's `PEER_AUTHENTICATION_TIMEOUT` for unauthenticated I/O idle
/// timeouts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboundTimeouts {
    /// Maximum time to establish the TCP connection.
    pub connect_secs: u64,
    /// Maximum idle time per unauthenticated receive (recv_hello, recv_auth).
    pub auth_secs: u64,
}

impl OutboundTimeouts {
    pub fn from_config(config: &OverlayConfig) -> Self {
        Self {
            connect_secs: config.connect_timeout_secs,
            auth_secs: config.auth_timeout_secs,
        }
    }
}

/// Configuration for the overlay network.
///
/// Controls peer connection limits, timeouts, and network-specific settings.
/// Use [`OverlayConfig::testnet()`] or [`OverlayConfig::mainnet()`] for
/// pre-configured network settings.
///
/// # Example
///
/// ```rust
/// use henyey_overlay::{OverlayConfig, PeerAddress};
///
/// let mut config = OverlayConfig::testnet();
/// config.max_inbound_peers = 32;
/// config.preferred_peers.push(PeerAddress::new("my-validator.example.com", 11625));
/// ```
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Maximum number of inbound peer connections to accept.
    ///
    /// When this limit is reached, new incoming connections are rejected.
    pub max_inbound_peers: usize,

    /// Maximum number of outbound peer connections.
    ///
    /// The overlay will not initiate more than this many connections.
    pub max_outbound_peers: usize,

    /// Target number of outbound connections to maintain.
    ///
    /// The overlay manager will attempt to maintain at least this many
    /// outbound connections by connecting to known peers.
    pub target_outbound_peers: usize,

    /// Port to listen on for incoming connections.
    ///
    /// Standard Stellar port is 11625.
    pub listen_port: u16,

    /// Known peers to connect to on startup.
    ///
    /// The overlay manager will attempt to connect to these peers
    /// and will use them for initial network bootstrap.
    pub known_peers: Vec<PeerAddress>,

    /// Preferred peers that should always be connected.
    ///
    /// These peers are given priority when establishing outbound
    /// connections and will be reconnected if disconnected.
    pub preferred_peers: Vec<PeerAddress>,

    /// Preferred peer public keys for node-ID-based preference.
    ///
    /// Peers whose authenticated node ID matches any of these keys are treated
    /// as preferred, regardless of their network address. Matches stellar-core's
    /// `PREFERRED_PEER_KEYS`.
    pub preferred_peer_keys: HashSet<PeerId>,

    /// When `true`, reject any authenticated peer that is not preferred
    /// (by address or by key). Matches stellar-core's `PREFERRED_PEERS_ONLY`.
    pub preferred_peers_only: bool,

    /// Network passphrase for authentication.
    ///
    /// Must match the network you're connecting to. Used to derive
    /// the network ID for signing authentication certificates.
    pub network_passphrase: String,

    /// Peer authentication timeout in seconds.
    ///
    /// Maximum idle time per unauthenticated receive operation (recv_hello,
    /// recv_auth). Each receive independently times out after this duration.
    /// Matches stellar-core's `Peer::getIOTimeout()` which returns
    /// `PEER_AUTHENTICATION_TIMEOUT` (2s) for unauthenticated I/O idle
    /// timeouts. The separate 30s idle timeout for authenticated peers is
    /// enforced in the peer loop.
    pub auth_timeout_secs: u64,

    /// Connection timeout in seconds.
    ///
    /// Maximum time to wait for TCP connection establishment.
    pub connect_timeout_secs: u64,

    /// Message flood TTL in seconds.
    ///
    /// How long to remember seen messages for duplicate detection.
    pub flood_ttl_secs: u64,

    /// Whether to listen for incoming connections.
    ///
    /// Set to `false` for sync-only nodes that don't accept inbound peers.
    pub listen_enabled: bool,

    /// Whether this node is a validator (participates in consensus).
    ///
    /// When `false` (watcher mode), the overlay filters out non-essential
    /// flood messages (Transaction, FloodAdvert, FloodDemand, and Survey
    /// messages) before they enter the broadcast channel, reducing channel
    /// pressure by ~90% on mainnet. SCP and fetch messages are always kept.
    pub is_validator: bool,

    /// Version info string for Hello messages.
    ///
    /// Identifies this node to peers during handshake.
    pub version_string: String,

    /// Optional channel for peer connection events.
    ///
    /// If set, the overlay manager will send [`PeerEvent`] notifications
    /// when peers connect or disconnect.
    pub peer_event_tx: Option<mpsc::Sender<PeerEvent>>,

    /// Flow control byte parameters (initial grant and batch size).
    ///
    /// Controls the per-peer byte-level flow control behavior. When [`Auto`](FlowControlBytesConfig::Auto),
    /// values are auto-computed from max tx size. When [`Fixed`](FlowControlBytesConfig::Fixed),
    /// operator-supplied overrides are used directly.
    pub flow_control_bytes_config: FlowControlBytesConfig,
}

/// Peer connection events emitted by the overlay.
///
/// These events are sent via the `peer_event_tx` channel in [`OverlayConfig`]
/// to notify external components about peer connection state changes.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// A peer successfully connected and completed authentication.
    Connected(PeerAddress, PeerType),
    /// A connection attempt to a peer failed.
    Failed(PeerAddress, PeerType),
}

/// Categorizes whether a peer connection was initiated by us or by them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerType {
    /// The peer connected to us (we accepted the connection).
    Inbound,
    /// We connected to the peer (we initiated the connection).
    Outbound,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            max_inbound_peers: 64,
            max_outbound_peers: 8,
            target_outbound_peers: 8,
            listen_port: 11625,
            known_peers: Vec::new(),
            preferred_peers: Vec::new(),
            preferred_peer_keys: HashSet::new(),
            preferred_peers_only: false,
            network_passphrase: "Test SDF Network ; September 2015".to_string(),
            auth_timeout_secs: 2,
            connect_timeout_secs: 10,
            flood_ttl_secs: 300,
            listen_enabled: true,
            is_validator: true,
            version_string: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
            peer_event_tx: None,
            flow_control_bytes_config: FlowControlBytesConfig::default(),
        }
    }
}

impl OverlayConfig {
    /// Create testnet configuration with known testnet peers.
    pub fn testnet() -> Self {
        Self {
            known_peers: vec![
                PeerAddress::new("core-testnet1.stellar.org", 11625),
                PeerAddress::new("core-testnet2.stellar.org", 11625),
                PeerAddress::new("core-testnet3.stellar.org", 11625),
            ],
            network_passphrase: "Test SDF Network ; September 2015".to_string(),
            listen_enabled: false, // Don't listen by default for sync-only
            ..Default::default()
        }
    }

    /// Create mainnet configuration with known mainnet peers.
    ///
    /// Includes all Tier 1 validators from the Stellar network for better
    /// peer diversity and resilience against individual nodes being overloaded.
    pub fn mainnet() -> Self {
        Self {
            known_peers: vec![
                // SDF
                PeerAddress::new("core-live-a.stellar.org", 11625),
                PeerAddress::new("core-live-b.stellar.org", 11625),
                PeerAddress::new("core-live-c.stellar.org", 11625),
                // LOBSTR
                PeerAddress::new("v1.stellar.lobstr.co", 11625),
                PeerAddress::new("v2.stellar.lobstr.co", 11625),
                PeerAddress::new("v3.stellar.lobstr.co", 11625),
                PeerAddress::new("v4.stellar.lobstr.co", 11625),
                PeerAddress::new("v5.stellar.lobstr.co", 11625),
                // SatoshiPay
                PeerAddress::new("stellar-de-fra.satoshipay.io", 11625),
                PeerAddress::new("stellar-sg-sin.satoshipay.io", 11625),
                PeerAddress::new("stellar-us-iowa.satoshipay.io", 11625),
                // Blockdaemon
                PeerAddress::new("stellar-full-validator1.bdnodes.net", 11625),
                PeerAddress::new("stellar-full-validator2.bdnodes.net", 11625),
                PeerAddress::new("stellar-full-validator3.bdnodes.net", 11625),
                // Franklin Templeton
                PeerAddress::new("stellar1.franklintempleton.com", 11625),
                PeerAddress::new("stellar2.franklintempleton.com", 11625),
                PeerAddress::new("stellar3.franklintempleton.com", 11625),
                // PublicNode
                PeerAddress::new("bootes.publicnode.org", 11625),
                PeerAddress::new("lyra.publicnode.org", 11625),
                PeerAddress::new("hercules.publicnode.org", 11625),
            ],
            network_passphrase: "Public Global Stellar Network ; September 2015".to_string(),
            listen_enabled: false,
            ..Default::default()
        }
    }
}

/// Address of a peer on the network.
///
/// Represents a network endpoint that can be used to connect to a Stellar node.
/// The host can be either an IP address or a hostname.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddress {
    /// IP address or hostname of the peer.
    pub host: String,
    /// TCP port number (standard Stellar port is 11625).
    pub port: u16,
}

impl PeerAddress {
    /// Creates a new peer address from a host and port.
    ///
    /// # Example
    ///
    /// ```rust
    /// use henyey_overlay::PeerAddress;
    ///
    /// let addr = PeerAddress::new("core-testnet1.stellar.org", 11625);
    /// assert_eq!(addr.to_string(), "core-testnet1.stellar.org:11625");
    /// ```
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// Returns a socket address string suitable for TCP connection.
    ///
    /// The format is `host:port` which can be passed to `TcpStream::connect`.
    pub fn to_socket_addr(&self) -> String {
        self.to_string()
    }

    /// Returns true if this address is a private/local network address.
    ///
    /// Private addresses include:
    /// - 10.0.0.0/8 (10.x.x.x)
    /// - 172.16.0.0/12 (172.16-31.x.x)
    /// - 192.168.0.0/16 (192.168.x.x)
    /// - 127.0.0.0/8 (localhost)
    /// - 169.254.0.0/16 (link-local)
    /// - 224.0.0.0/4 (multicast)
    /// - 0.0.0.0 (unspecified)
    /// - ::1, fe80::/10, fc00::/7, multicast, unspecified (IPv6)
    ///
    /// These addresses should not be shared with other peers as they
    /// are not routable on the public internet.
    // SECURITY: private-address filtering is a network-layer concern; not a consensus code bug
    pub fn is_private(&self) -> bool {
        use std::net::IpAddr;

        // Try to parse as IP address
        if let Ok(ip) = self.host.parse::<IpAddr>() {
            match ip {
                IpAddr::V4(v4) => {
                    v4.is_private()
                        || v4.is_loopback()
                        || v4.is_link_local()
                        || v4.is_multicast()
                        || v4.is_unspecified()
                }
                IpAddr::V6(v6) => {
                    let first_segment = v6.segments()[0];
                    v6.is_loopback()
                        || v6.is_multicast()
                        || v6.is_unspecified()
                        || ((first_segment & 0xffc0) == 0xfe80)
                        || ((first_segment & 0xfe00) == 0xfc00)
                }
            }
        } else {
            // Hostname - check for localhost
            self.host == "localhost"
        }
    }
}

impl std::fmt::Display for PeerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl From<std::net::SocketAddr> for PeerAddress {
    fn from(addr: std::net::SocketAddr) -> Self {
        Self {
            host: addr.ip().to_string(),
            port: addr.port(),
        }
    }
}

/// Unique identifier for a peer based on their Ed25519 public key.
///
/// Each Stellar node has a cryptographic identity derived from its secret key.
/// The `PeerId` wraps the node's public key and provides methods for comparison,
/// hashing, and display.
///
/// # Display Format
///
/// When displayed, the full strkey format (G...) is shown for easy identification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId(pub stellar_xdr::curr::PublicKey);

impl PeerId {
    /// Creates a `PeerId` from an XDR public key.
    pub fn from_xdr(key: stellar_xdr::curr::PublicKey) -> Self {
        Self(key)
    }

    /// Creates a `PeerId` from raw Ed25519 public key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(bytes),
        ))
    }

    /// Returns a reference to the raw 32-byte public key.
    pub fn as_bytes(&self) -> &[u8; 32] {
        match &self.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                bytes,
            )) => bytes,
        }
    }

    /// Returns the full hex-encoded representation of the public key.
    pub fn to_hex(&self) -> String {
        hex::encode(self.as_bytes())
    }

    /// Returns the Stellar strkey-encoded representation of the public key.
    ///
    /// The strkey format is the standard way to represent Stellar public keys
    /// as human-readable strings starting with 'G'.
    pub fn to_strkey(&self) -> String {
        stellar_strkey::ed25519::PublicKey(*self.as_bytes()).to_string()
    }

    /// Creates a `PeerId` from a Stellar strkey-encoded public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not a valid Stellar public key strkey.
    pub fn from_strkey(strkey: &str) -> Result<Self> {
        let pk: stellar_strkey::ed25519::PublicKey = strkey
            .parse()
            .map_err(|e| OverlayError::InvalidPeerAddress(format!("Invalid strkey: {}", e)))?;
        Ok(Self::from_bytes(pk.0))
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display strkey format (G...)
        write!(f, "{}", self.to_strkey())
    }
}

/// Trait for handling incoming messages from the overlay network.
///
/// Implement this trait to process Stellar network messages received from peers.
/// The handler is called for each message after it passes through the flood gate
/// and authentication checks.
#[async_trait::async_trait]
pub trait MessageHandler: Send + Sync {
    /// Handles an incoming message from a peer.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The identity of the peer that sent the message
    /// * `message` - The Stellar message received
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if message processing failed.
    async fn handle_message(
        &self,
        peer_id: &PeerId,
        message: stellar_xdr::curr::StellarMessage,
    ) -> Result<()>;
}

/// Local node identity and configuration for overlay authentication.
///
/// Contains the cryptographic identity (secret key) and protocol version
/// information that is exchanged with peers during the Hello handshake.
///
/// # Example
///
/// ```rust
/// use henyey_overlay::LocalNode;
/// use henyey_crypto::SecretKey;
///
/// let secret_key = SecretKey::generate();
/// let node = LocalNode::new_testnet(secret_key);
/// println!("Node ID: {}", node.peer_id());
/// ```
#[derive(Clone)]
pub struct LocalNode {
    /// Ed25519 secret key used for signing authentication certificates.
    pub secret_key: henyey_crypto::SecretKey,

    /// Network ID derived from the network passphrase.
    ///
    /// Used to ensure peers are on the same network (testnet vs mainnet).
    pub network_id: henyey_common::NetworkId,

    /// Version string sent in Hello messages.
    ///
    /// Typically includes the software name and version.
    pub version_string: String,

    /// Ledger protocol version this node supports.
    pub ledger_version: u32,

    /// Overlay protocol version this node uses.
    pub overlay_version: u32,

    /// Minimum overlay protocol version this node accepts from peers.
    ///
    /// Peers with lower versions will be rejected during handshake.
    pub overlay_min_version: u32,

    /// Port this node listens on for incoming peer connections.
    ///
    /// Sent to peers in Hello messages so they know how to connect back.
    pub listening_port: u16,
}

const LEDGER_VERSION: u32 = henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;
// Parity: stellar-core/src/main/Config.cpp:164-165 (v26.0.0)
// OVERLAY_PROTOCOL_VERSION = 40, OVERLAY_PROTOCOL_MIN_VERSION = 38
const OVERLAY_VERSION: u32 = 40;
const OVERLAY_MIN_VERSION: u32 = 38;
const DEFAULT_LISTENING_PORT: u16 = 11625;

impl LocalNode {
    fn with_network(
        secret_key: henyey_crypto::SecretKey,
        network_id: henyey_common::NetworkId,
    ) -> Self {
        Self {
            secret_key,
            network_id,
            version_string: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
            ledger_version: LEDGER_VERSION,
            overlay_version: OVERLAY_VERSION,
            overlay_min_version: OVERLAY_MIN_VERSION,
            listening_port: DEFAULT_LISTENING_PORT,
        }
    }

    /// Set the version string to include a commit hash for P2P identification.
    ///
    /// Called at startup when the commit hash is available from build metadata.
    pub fn set_commit_hash(&mut self, commit_hash: &str) {
        self.version_string = henyey_common::version::build_version_string_full(
            env!("CARGO_PKG_VERSION"),
            commit_hash,
        );
    }

    /// Creates a new local node configured for the Stellar testnet.
    ///
    /// Uses the testnet network passphrase and current protocol versions.
    pub fn new_testnet(secret_key: henyey_crypto::SecretKey) -> Self {
        Self::with_network(secret_key, henyey_common::NetworkId::testnet())
    }

    /// Creates a new local node configured for the Stellar mainnet.
    ///
    /// Uses the mainnet network passphrase and current protocol versions.
    pub fn new_mainnet(secret_key: henyey_crypto::SecretKey) -> Self {
        Self::with_network(secret_key, henyey_common::NetworkId::mainnet())
    }

    /// Creates a new local node with a custom network passphrase.
    ///
    /// Use this for standalone networks or other non-standard deployments.
    pub fn new(secret_key: henyey_crypto::SecretKey, network_passphrase: &str) -> Self {
        Self::with_network(
            secret_key,
            henyey_common::NetworkId::from_passphrase(network_passphrase),
        )
    }

    /// Returns this node's public key.
    pub fn public_key(&self) -> henyey_crypto::PublicKey {
        self.secret_key.public_key()
    }

    /// Returns this node's public key in XDR format.
    pub fn xdr_public_key(&self) -> stellar_xdr::curr::PublicKey {
        (&self.public_key()).into()
    }

    /// Returns this node's peer ID (derived from public key).
    pub fn peer_id(&self) -> PeerId {
        PeerId::from_xdr(self.xdr_public_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_address() {
        let addr = PeerAddress::new("127.0.0.1", 11625);
        assert_eq!(addr.to_socket_addr(), "127.0.0.1:11625");
        assert_eq!(addr.to_string(), "127.0.0.1:11625");
    }

    #[test]
    fn test_overlay_config_testnet() {
        let config = OverlayConfig::testnet();
        assert_eq!(config.known_peers.len(), 3);
        assert_eq!(
            config.network_passphrase,
            "Test SDF Network ; September 2015"
        );
    }

    #[test]
    fn test_auth_timeout_matches_upstream_g2() {
        // G2: stellar-core Peer::getIOTimeout() returns 2s for unauthenticated
        // peers (during handshake). The default auth_timeout_secs must be 2.
        let config = OverlayConfig::default();
        assert_eq!(
            config.auth_timeout_secs, 2,
            "auth_timeout_secs should be 2s matching stellar-core getIOTimeout()"
        );
    }

    #[test]
    fn test_peer_id() {
        let bytes = [1u8; 32];
        let peer_id = PeerId::from_bytes(bytes);
        assert_eq!(peer_id.as_bytes(), &bytes);
    }

    #[test]
    fn test_local_node() {
        let secret = henyey_crypto::SecretKey::generate();
        let node = LocalNode::new_testnet(secret);
        let peer_id = node.peer_id();
        assert!(!peer_id.to_hex().is_empty());
    }
}
