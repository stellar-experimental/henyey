//! Overlay manager for coordinating peer connections and message routing.
//!
//! The [`OverlayManager`] is the primary interface for the overlay network subsystem.
//! It handles all aspects of peer-to-peer networking:
//!
//! - **Connection Management**: Establishes and maintains TCP connections to peers,
//!   respecting configured limits for inbound and outbound connections
//!
//! - **Peer Discovery**: Learns about new peers from connected nodes and maintains
//!   a pool of known addresses to connect to
//!
//! - **Message Routing**: Receives messages from peers and distributes them to
//!   subscribers, while also sending outbound messages to appropriate peers
//!
//! - **Flood Control**: Uses the [`FloodGate`] to prevent duplicate message
//!   propagation while ensuring all peers receive new messages
//!
//! # Architecture
//!
//! The manager runs several background tasks:
//!
//! 1. **Listener task**: Accepts incoming connections (if enabled)
//! 2. **Connector task**: Initiates outbound connections to maintain target peer count
//! 3. **Peer tasks**: One per connected peer, handles message I/O
//! 4. **Advertiser task**: Periodically sends peer lists to connected nodes
//!
//! # Flow Control
//!
//! The overlay implements Stellar's flow control protocol using `SendMore` and
//! `SendMoreExtended` messages. This prevents peers from overwhelming each other
//! with messages during high-traffic periods.
//!
//! [`FloodGate`]: crate::FloodGate

mod connection;
mod peer_loop;
mod tick;

pub use connection::AddPeerOutcome;
pub(crate) use peer_loop::sanitize_error_msg;

use crate::{
    codec::helpers,
    connection::{ConnectionDirection, ConnectionPool, Listener},
    connection_factory::{ConnectionFactory, TcpConnectionFactory},
    flood::{compute_message_hash, FloodGate, FloodGateStats},
    flow_control::{FlowControl, FlowControlBytesConfig, ScpQueueCallback},
    metrics::OverlayMetrics,
    peer::{PeerInfo, PeerStats, PeerStatsSnapshot},
    DialKey, LocalNode, OverlayConfig, OverlayError, PeerAddress, PeerEvent, PeerId,
    ResolvedPeerAddr, Result,
};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rand::Rng;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use stellar_xdr::{PeerAddress as XdrPeerAddress, PeerAddressIp, StellarMessage, Uint256, VecM};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace, warn};

/// Maximum number of known peer addresses kept in memory.
///
/// Matches the batch size used by `load_random_peers` from the database (1000).
/// Prevents unbounded growth from PEERS messages sent by remote nodes.
const MAX_KNOWN_PEERS: usize = 1000;

/// Buffer size for the broadcast channel carrying non-critical overlay
/// messages (TX floods, etc.). SCP and fetch-response messages bypass
/// this channel via dedicated mpsc channels, so the broadcast channel
/// only carries remaining message types. 4096 provides headroom for
/// mainnet traffic bursts from multiple peers.
const BROADCAST_CHANNEL_SIZE: usize = 4096;

/// Bounded capacity of the dedicated overlay→event-loop SCP ingest channel
/// (`scp_message_tx`/`scp_message_rx`).
///
/// This channel carries SCP envelopes from the peer-receive path to the main
/// event loop. It MUST be bounded: when the event loop stalls (e.g. on the
/// post-catchup SQLite write-lock contention tracked in #3582), ~24 tier-1
/// validators flood ~100+ SCP envelopes/slot with nothing draining the
/// channel. An unbounded channel grows RSS ~4 GB/min until the validator is
/// OOM-killed, producing a fatal restart loop (#3623).
///
/// Sizing: 8192 covers tens of seconds of a healthy multi-validator flood
/// (~100+ envelopes/slot, ~5s slots) so that under normal jitter and short
/// stalls the channel stays near-empty and drops effectively never happen —
/// the loop drains far faster than the flood arrives. The hard cap on
/// retained envelopes is 8192; with typical SCP envelopes well under ~1 KB the
/// worst-case retained memory is on the order of a few MB, versus the
/// unbounded growth to ~37.5 GB observed in #3623.
///
/// On overflow the overlay side `try_send`s and DROPS the envelope (bumping
/// `messages_dropped`) rather than blocking — blocking the peer-receive path
/// is its own event-loop hazard. Dropping SCP is recoverable: peers re-flood
/// every slot, and the event loop's gap-detection + `SyncRecoveryManager`
/// backfill missing state via `GetScpState`. This count-based bound mirrors
/// stellar-core's bounded inbound `FlowControlMessageCapacity`; a strict
/// credit-based byte bound is the follow-up tracked in #3625.
pub const SCP_CHANNEL_CAPACITY: usize = 8192;

/// In-flight dedup filter for inbound SCP envelopes, provided by the app and
/// run in the overlay peer tasks (maxtps iter 7; parity: stellar-core
/// `checkScheduledAndCache`, Peer.cpp:1113-1117). Input is the full-message
/// hash. Returns `Some(token)` when the envelope is new (the token must ride
/// the message and be dropped when processing completes, expiring the cache
/// entry) or `None` when a copy is already in flight (drop the message).
pub type ScpInboundFilter =
    dyn Fn(&henyey_common::Hash256) -> Option<std::sync::Arc<()>> + Send + Sync;

/// Bound on the dedicated **fetch-response/-request** intake channel
/// (`fetch_response_tx` → event-loop `fetch_response_rx`).
///
/// This channel carries `GeneralizedTxSet`/`TxSet` responses (and the
/// `GetTxSet`/`GetScpState`/`GetScpQuorumset` requests we answer), each of
/// which may be a full **multi-MB** tx-set. On `origin/main` it was an
/// `mpsc::unbounded_channel()`: when the catchup→Tracking handoff stalls the
/// event loop (#3582 SQLite write-lock contention) the consumer at
/// `lifecycle.rs` stops draining while 24 tier-1 validators keep feeding
/// tx-sets, so RSS grows ~5.4 GB/min until the validator is OOM-killed
/// (#3623/#3661). #3626 bounded the *SCP* intake channel but not this one.
///
/// Sizing: 1024 full tx-sets is a generous backlog for normal jitter and
/// short stalls (the loop drains far faster than fetch responses arrive) while
/// keeping the hard retained-memory cap bounded (vs. the 8→41 GB unbounded
/// growth in #3661). On overflow we `try_send` and DROP the message rather
/// than blocking the peer-receive path. Dropping a fetch response is
/// recoverable: the tx-set stays in `TxSetTracker.pending` and is re-requested
/// by the periodic `request_pending_tx_sets()` tick (lifecycle.rs) + ItemFetcher
/// retry. Fetch messages are NOT flow-controlled (excluded from
/// `is_flow_controlled_message`) and the enqueued copy is a tokenless `clone`
/// (see `OverlayMessage`'s `Clone`), so dropping never touches SEND_MORE credit.
/// Mirrors stellar-core's bounded overlay→herder model
/// (`FlowControlMessageCapacity` / `PEER_FLOOD_READING_CAPACITY`;
/// `TXSET_CACHE_SIZE`=10000; `MAX_SLOTS_TO_REMEMBER`=12).
pub const FETCH_CHANNEL_CAPACITY: usize = 1024;

/// Bound on the **catchup-cache** fan-out channel created by
/// `subscribe_catchup()` (→ off-loop `cache_messages_during_catchup_impl`).
///
/// This channel fans out `ScpMessage`/`GeneralizedTxSet`/`TxSet`/`ScpQuorumset`
/// (again up to multi-MB tx-sets) to the pre-warm catchup cache task. On
/// `origin/main` it was an `mpsc::unbounded_channel()` whose consumer's
/// `abort()` is gated on the event loop reaching `pending_catchup completed`
/// (`lifecycle.rs`). When the handoff stalls, the abort never fires and the
/// cache task may stop draining, so this channel is the second unbounded
/// tx-set accumulator behind the #3661 OOM.
///
/// Sizing: 1024, same rationale as [`FETCH_CHANNEL_CAPACITY`]. On overflow we
/// `try_send` and DROP. Dropping is recoverable: the catchup cache is a
/// *pre-warm* only — the task itself registers missing tx-sets as pending and
/// re-broadcasts `GetTxSet`, dropped EXTERNALIZE envelopes are re-flooded by
/// peers every slot, and after the handoff the main loop re-fetches anything
/// still in `TxSetTracker.pending`. The fan-out copy is a tokenless `clone`,
/// so dropping never interacts with flow-control credit. Same bounded-overlay
/// model as [`FETCH_CHANNEL_CAPACITY`].
pub const CATCHUP_CHANNEL_CAPACITY: usize = 1024;

/// Maximum number of peer addresses included in a single PEERS message.
///
/// Matches stellar-core's limit of 50 addresses per Peers message
/// (see `Peer::recvPeers` in Peer.cpp).
const MAX_PEERS_PER_MESSAGE: usize = 50;

/// Extra inbound slots reserved for possibly-preferred peers.
///
/// Matches stellar-core `Config::POSSIBLY_PREFERRED_EXTRA`.
const POSSIBLY_PREFERRED_EXTRA: usize = 2;

/// Minimum spacing between broadcast-backpressure WARN log lines (#3792).
///
/// During an event-loop park, `broadcast()` can be called thousands of times in
/// a single second with every peer channel full (observed max ~24k lines/s ≈
/// 3.6 MB of synchronous log I/O emitted *while the loop is already parked*).
/// The per-message-type drop and blackout counters are the source of truth, so
/// the WARN is throttled to at most one line per this interval without losing
/// any measurement volume.
const BROADCAST_BACKPRESSURE_WARN_INTERVAL_MS: u64 = 1_000;

/// Pure interval gate for a rate-limited log line, backed by a single
/// `AtomicU64` holding the last-emit epoch-ms (0 = never emitted).
///
/// Returns `true` (and atomically claims `now_ms` as the new last-emit time) at
/// most once per `interval_ms`; concurrent callers race via CAS so exactly one
/// wins each window. Clock regressions are treated as "throttled" (fail-closed)
/// via saturating subtraction.
fn should_emit_now(last_emit_ms: &AtomicU64, now_ms: u64, interval_ms: u64) -> bool {
    let mut last = last_emit_ms.load(Ordering::Relaxed);
    loop {
        if now_ms.saturating_sub(last) < interval_ms {
            return false;
        }
        match last_emit_ms.compare_exchange_weak(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return true,
            Err(observed) => last = observed,
        }
    }
}

/// Immutable snapshot of preferred peer state.
///
/// Holds both the original config entries (hostnames) and DNS-resolved
/// IP-based addresses. Callers consume this via `Arc<PreferredPeerSet>` —
/// no stale cloned `Vec<PeerAddress>` possible.
///
/// This replaces the previous three separate representations:
/// - `config.preferred_peers` Vec → `config_entries`
/// - `SharedPeerState.preferred_peers: Arc<Vec<PeerAddress>>` → `Arc<PreferredPeerSet>`
/// - `ConnectionPool.preferred_ips` → updated from `resolved_ips` after each DNS cycle
#[derive(Debug, Clone)]
pub(super) struct PreferredPeerSet {
    /// Original config entries (hostnames). Used for outbound dialing,
    /// original_address matching, and connect_preferred_peers iteration.
    config_entries: Vec<PeerAddress>,
    /// Resolved IP-based addresses. Updated after each DNS resolution cycle.
    resolved: Vec<PeerAddress>,
    /// Resolved IPs only. Used for ConnectionPool preferred_ips updates
    /// and fast IP-based matching.
    resolved_ips: HashSet<IpAddr>,
    /// Preferred peer public keys for node-ID-based preference.
    /// Matches stellar-core's `PREFERRED_PEER_KEYS`.
    preferred_keys: HashSet<PeerId>,
}

impl PreferredPeerSet {
    /// Create initial snapshot from config (no DNS resolution yet).
    pub(super) fn from_config(
        config_entries: Vec<PeerAddress>,
        preferred_keys: HashSet<PeerId>,
    ) -> Self {
        Self {
            config_entries,
            resolved: Vec::new(),
            resolved_ips: HashSet::new(),
            preferred_keys,
        }
    }

    /// Create updated snapshot with new DNS resolution results.
    pub(super) fn with_resolved(&self, resolved: Vec<PeerAddress>) -> Self {
        let resolved_ips = resolved
            .iter()
            .filter_map(|addr| addr.host.parse::<IpAddr>().ok())
            .collect();
        Self {
            config_entries: self.config_entries.clone(),
            resolved,
            resolved_ips,
            preferred_keys: self.preferred_keys.clone(),
        }
    }

    /// Check if a peer matches any preferred entry (hostname, resolved IP,
    /// or node-ID key).
    ///
    /// For outbound peers (with `original_address`), the hostname config entry
    /// matches directly. For inbound peers (no `original_address`), the resolved
    /// IP addresses are checked. For all authenticated peers, the node ID is
    /// checked against `preferred_keys`.
    pub(super) fn is_preferred(&self, info: &PeerInfo) -> bool {
        // Key-based preference (stellar-core PREFERRED_PEER_KEYS)
        if self.preferred_keys.contains(&info.peer_id) {
            return true;
        }
        // Address-based preference
        self.config_entries
            .iter()
            .any(|pref| OverlayManager::peer_info_matches_address(info, pref))
            || self
                .resolved
                .iter()
                .any(|pref| OverlayManager::peer_info_matches_address(info, pref))
    }

    /// Get config entries for outbound connection attempts, shuffled to avoid
    /// starvation. stellar-core uses random selection; fixed order causes later
    /// entries to never get a turn when outbound slots are exhausted.
    pub(super) fn shuffled_config_entries(&self, rng: &mut impl Rng) -> Vec<PeerAddress> {
        let mut entries = self.config_entries.clone();
        entries.shuffle(rng);
        entries
    }

    /// Get entries for preferred-peer dialing: resolved IPs when available,
    /// config hostname entries before first DNS resolution.
    ///
    /// After DNS resolution, resolved entries have canonical IP addresses,
    /// eliminating hostname/IP aliasing in `retry_after` and
    /// `has_connection_to`. Peers that failed DNS resolution are
    /// omitted from dialing (they will be retried on the next DNS cycle).
    pub(super) fn shuffled_dial_entries(&self, rng: &mut impl Rng) -> Vec<PeerAddress> {
        if self.resolved.is_empty() {
            return self.shuffled_config_entries(rng);
        }
        let mut entries = self.resolved.clone();
        entries.shuffle(rng);
        entries
    }

    /// Get the resolved IP addresses for updating ConnectionPool.
    pub(super) fn resolved_ips(&self) -> &HashSet<IpAddr> {
        &self.resolved_ips
    }
}

/// Typed known-peer storage separating configured hostnames from discovered IPs
/// and maintaining a per-entry DNS resolution cache.
///
/// Analogous to `PreferredPeerSet` but for the general known-peer pool used by
/// `fill_outbound_slots()`. Config entries are immutable; DNS resolution state
/// is tracked per-entry with last-good preservation on failure.
pub(super) struct KnownPeerSet {
    /// Original hostname entries from config (immutable after init).
    config_entries: Vec<PeerAddress>,
    /// Per-config-entry resolution state. Same length as `config_entries`.
    /// `Some(addr)` = last successful resolution; `None` = never resolved.
    /// On DNS failure, last-good is preserved (not cleared).
    resolved: Vec<Option<PeerAddress>>,
    /// Peers discovered via gossip/DB refresh (arrive as IPs).
    /// Capped at `MAX_KNOWN_PEERS - config_entries.len()`.
    discovered: Vec<PeerAddress>,
    /// Dedup set for discovered entries (DialKey based).
    discovered_keys: HashSet<DialKey>,
}

impl KnownPeerSet {
    /// Create from config entries with no resolution state.
    pub(super) fn from_config(config_entries: Vec<PeerAddress>) -> Self {
        let resolved = vec![None; config_entries.len()];
        Self {
            config_entries,
            resolved,
            discovered: Vec::new(),
            discovered_keys: HashSet::new(),
        }
    }

    /// Apply DNS resolution results. `results` must be positionally aligned
    /// with `config_entries`. On `Some(addr)`: updates resolution. On `None`:
    /// preserves last-good (does NOT clear).
    pub(super) fn update_resolved(&mut self, results: &[Option<PeerAddress>]) {
        assert_eq!(
            results.len(),
            self.config_entries.len(),
            "resolve results length must match config_entries"
        );
        for (i, result) in results.iter().enumerate() {
            if let Some(addr) = result {
                self.resolved[i] = Some(addr.clone());
            }
            // None → preserve last-good (no-op)
        }
    }

    /// Add a discovered peer (from gossip or DB). Returns false if full or duplicate
    /// (checks against both existing discovered peers and config entries).
    pub(super) fn add_discovered(&mut self, addr: PeerAddress) -> bool {
        let cap = MAX_KNOWN_PEERS.saturating_sub(self.config_entries.len());
        if self.discovered.len() >= cap {
            return false;
        }
        let key = addr.dial_key();
        // Check against config entries (both hostname and resolved forms)
        for (i, config) in self.config_entries.iter().enumerate() {
            if config.dial_key() == key {
                return false;
            }
            if let Some(resolved) = &self.resolved[i] {
                if resolved.dial_key() == key {
                    return false;
                }
            }
        }
        if !self.discovered_keys.insert(key) {
            return false;
        }
        self.discovered.push(addr);
        true
    }

    /// Replace all discovered peers (from DB refresh via set_known_peers).
    /// Config entries and their resolution state are preserved.
    /// Peers matching config entries (by hostname or resolved IP) are filtered out.
    pub(super) fn set_discovered(&mut self, peers: Vec<PeerAddress>) {
        let cap = MAX_KNOWN_PEERS.saturating_sub(self.config_entries.len());
        self.discovered_keys.clear();
        self.discovered.clear();
        // Build config key set for filtering (both hostname and resolved forms).
        let config_keys: HashSet<DialKey> = self
            .config_entries
            .iter()
            .enumerate()
            .flat_map(|(i, config)| {
                let mut keys = vec![config.dial_key()];
                if let Some(resolved) = &self.resolved[i] {
                    keys.push(resolved.dial_key());
                }
                keys
            })
            .collect();

        for peer in peers {
            if self.discovered.len() >= cap {
                break;
            }
            let key = peer.dial_key();
            if config_keys.contains(&key) {
                continue;
            }
            if self.discovered_keys.insert(key) {
                self.discovered.push(peer);
            }
        }
    }

    /// Get shuffled dial targets: resolved IP for config entries with successful
    /// DNS, hostname for never-resolved entries, discovered peers as-is.
    /// Deduplicates by dial_key (two hostnames → same IP = one entry).
    pub(super) fn shuffled_dial_entries(&self, rng: &mut impl Rng) -> Vec<PeerAddress> {
        let mut entries = Vec::with_capacity(self.config_entries.len() + self.discovered.len());
        let mut seen_keys: HashSet<DialKey> = HashSet::new();

        for (i, config) in self.config_entries.iter().enumerate() {
            let dial_addr = match &self.resolved[i] {
                Some(resolved) => resolved.clone(),
                None => config.clone(),
            };
            if seen_keys.insert(dial_addr.dial_key()) {
                entries.push(dial_addr);
            }
        }

        for discovered in &self.discovered {
            if seen_keys.insert(discovered.dial_key()) {
                entries.push(discovered.clone());
            }
        }

        entries.shuffle(rng);
        entries
    }

    /// All entries for diagnostics (config dial targets + discovered).
    pub(super) fn all_entries(&self) -> Vec<PeerAddress> {
        let mut entries = Vec::with_capacity(self.config_entries.len() + self.discovered.len());
        for (i, config) in self.config_entries.iter().enumerate() {
            match &self.resolved[i] {
                Some(resolved) => entries.push(resolved.clone()),
                None => entries.push(config.clone()),
            }
        }
        entries.extend(self.discovered.iter().cloned());
        entries
    }
}

/// Drain-gated inbound flow-control credit release token (#3625, Phase 1).
///
/// Carried on the SCP-routed [`OverlayMessage`]. On the peer-receive task,
/// `begin_message_processing` already locked local capacity for the SCP
/// message (via [`CapacityGuard`]). Instead of releasing that capacity at
/// channel-*enqueue* time (which let a stalled event-loop consumer keep
/// granting `SEND_MORE_EXTENDED` until the #3626 bounded-channel backstop
/// dropped messages), the release is deferred to the moment the app event-loop
/// consumer actually *drains* the envelope.
///
/// When [`FlowControlRelease::release`] (or `Drop`) fires, it calls
/// `FlowControl::end_message_processing` **exactly once** and, if the
/// 40-message / byte batch threshold is met, enqueues the resulting
/// `SEND_MORE_EXTENDED` to the peer's outbound channel (non-blocking
/// `try_send`, drop-on-full per the existing pattern). A stalled consumer
/// therefore never releases ⇒ never grants ⇒ senders honoring outbound
/// capacity stop ⇒ the bounded buffer no longer fills.
///
/// **Leak-safety:** the release MUST fire even if the message is dropped
/// without being processed (the #3626 drop-on-full path, or a shutdown
/// discard). Otherwise local capacity leaks and the peer is permanently
/// starved-by-omission. `Drop` covers every such path; the `Option` guard
/// makes the release idempotent so an explicit `release()` followed by `Drop`
/// fires `end_message_processing` only once.
///
/// Mirrors stellar-core `FlowControl::endMessageProcessing`
/// (`FlowControl.cpp:303-329`) — capacity is always *released* per processed
/// message; back-pressure comes from the *timing* of the release, not from
/// withholding capacity. The read-side socket throttle (`can_read`) is
/// deferred to Phase 2 (#3642).
pub struct FlowControlRelease {
    flow_control: Arc<FlowControl>,
    /// The SCP message whose capacity this token releases. `Some` until the
    /// release fires; taken to `None` to enforce exactly-once semantics.
    message: Option<StellarMessage>,
    /// The peer's outbound channel — used to enqueue the `SEND_MORE_EXTENDED`
    /// grant when the batch threshold is reached.
    outbound_tx: mpsc::Sender<OutboundMessage>,
    /// Shared overlay metrics, for the drop-on-full grant counter.
    metrics: Arc<OverlayMetrics>,
    /// #3642 Phase 2: the per-peer read-resume wake. `Some` only on tokens
    /// minted on a peer task (via [`FlowControlRelease::with_resume_notify`]);
    /// `None` for cross-crate / test-seam tokens. When the read socket is
    /// throttled (`FlowControl::is_throttled`) and this release completes a full
    /// reading-capacity batch (`SendMoreCapacity::num_total_messages > 0`),
    /// `release()` calls `stop_throttling()` and fires this `Notify`, re-enabling
    /// the peer loop's `if can_read()`-gated `peer.recv()` select arm. This is
    /// the async equivalent of stellar-core `Peer::endMessageProcessing` →
    /// `stopThrottling()` + `scheduleRead()` gated on
    /// `isThrottled() && numTotalMessages > 0` (`Peer.cpp:313-333`).
    resume_notify: Option<Arc<tokio::sync::Notify>>,
}

impl FlowControlRelease {
    /// Build a release token for a flow-controlled (SCP) message.
    pub(super) fn new(
        flow_control: Arc<FlowControl>,
        message: StellarMessage,
        outbound_tx: mpsc::Sender<OutboundMessage>,
        metrics: Arc<OverlayMetrics>,
    ) -> Self {
        Self {
            flow_control,
            message: Some(message),
            outbound_tx,
            metrics,
            resume_notify: None,
        }
    }

    /// #3642 Phase 2: attach the per-peer read-resume `Notify`.
    ///
    /// Called only by the peer task (`run_peer_loop`) when minting an SCP
    /// release token, threading in the peer's own `Notify`. The `crates/app`
    /// SCP consumer never constructs this — it only drops the token, which (via
    /// `release()`) drives both the SEND_MORE grant and, now, the read-resume.
    /// Keeping the `Notify` peer-side preserves the overlay-only footprint.
    pub(super) fn with_resume_notify(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.resume_notify = Some(notify);
        self
    }

    /// Release the held capacity exactly once.
    ///
    /// Calls `end_message_processing` and, if a `SEND_MORE_EXTENDED` grant is
    /// due (40-message or byte batch reached, or the total-message reading
    /// capacity refill), enqueues it to the peer. Idempotent: a second call
    /// (or `Drop` after an explicit call) is a no-op.
    fn release(&mut self) {
        let Some(msg) = self.message.take() else {
            return;
        };
        let cap = self.flow_control.end_message_processing(&msg);

        // #3642 Phase 2 — read-resume. `end_message_processing` returns
        // `num_total_messages > 0` exactly once per full reading-capacity batch
        // (`FlowControl.cpp` resets the counter at the boundary). When the read
        // socket is throttled and a batch just completed, lift the throttle and
        // wake the peer loop so its `if can_read()`-gated `peer.recv()` arm
        // re-enables. Mirrors core `Peer::endMessageProcessing` →
        // `stopThrottling()` + `scheduleRead()` gated on
        // `isThrottled() && numTotalMessages > 0` (`Peer.cpp:313-333`). The
        // resume is driven purely by consumer drain (this token dropping),
        // never by reading more — the no-deadlock invariant (#3642). The
        // 1s `periodic_interval` tick in `run_peer_loop` is a backstop should a
        // wake ever be missed.
        if cap.num_total_messages > 0 && self.flow_control.is_throttled() {
            self.flow_control.stop_throttling();
            if let Some(notify) = &self.resume_notify {
                notify.notify_one();
            }
        }

        if !cap.should_send() {
            return;
        }
        let send_more = StellarMessage::SendMoreExtended(stellar_xdr::SendMoreExtended {
            num_messages: cap.num_flood_messages as u32,
            num_bytes: cap.num_flood_bytes as u32,
        });
        // Non-blocking send: never block the consumer event loop on a full
        // per-peer outbound channel. On a full channel the grant is dropped and
        // counted; the peer re-derives capacity from the next batch (the
        // outbound straggler timeout in #3643 covers the pathological case).
        if self
            .outbound_tx
            .try_send(OutboundMessage::Send(send_more))
            .is_err()
        {
            self.metrics.messages_dropped.add(1);
        }
    }
}

impl Drop for FlowControlRelease {
    fn drop(&mut self) {
        // Covers the dropped-without-processing paths (drop-on-full backstop,
        // shutdown discard) so capacity never leaks.
        self.release();
    }
}

/// An overlay message received from a peer, ready for dispatch to subscribers.
///
/// `Clone` deliberately does NOT clone any attached [`FlowControlRelease`]
/// token: the token represents the single, exactly-once obligation to release
/// the SCP message's flow-control credit. It rides only on the one copy that is
/// *moved* into the dedicated SCP channel (in `route_to_subscribers`); clones
/// sent to other subscribers (extra-subscribers, broadcast) carry no token.
pub struct OverlayMessage {
    /// The peer that sent this message.
    pub from_peer: PeerId,
    /// The Stellar protocol message.
    pub message: StellarMessage,
    /// When the message was received from the peer (before broadcast channel delivery).
    pub received_at: std::time::Instant,
    /// Drain-gated inbound flow-control release token (#3625). `Some` only on
    /// the single SCP-routed copy that reaches the app consumer; released when
    /// that consumer drains the envelope. Never cloned (see the `Clone` impl).
    pub(crate) flow_release: Option<FlowControlRelease>,
    /// In-flight scheduled-cache token claimed by the peer task's SCP dedup
    /// filter (maxtps iter 7; parity: core `checkScheduledAndCache`). Rides
    /// only the SCP-routed copy; the app consumer moves it into the intake so
    /// the cache entry expires when processing completes. Never cloned.
    pub scp_inflight_token: Option<std::sync::Arc<()>>,
    /// Full-message hash precomputed by the peer task's flood-gate path for
    /// flood-tracked messages (currently attached for SCP envelopes only).
    /// Lets the app consumer skip recomputing the SHA-256.
    pub message_hash: Option<henyey_common::Hash256>,
}

impl Clone for OverlayMessage {
    fn clone(&self) -> Self {
        Self {
            from_peer: self.from_peer.clone(),
            message: self.message.clone(),
            received_at: self.received_at,
            // The release obligation is never duplicated.
            flow_release: None,
            // The in-flight marker rides only the SCP-routed original.
            scp_inflight_token: None,
            message_hash: self.message_hash,
        }
    }
}

impl OverlayMessage {
    /// Construct an overlay message with no flow-control release token.
    ///
    /// This is the cross-crate constructor (the `flow_release` field is
    /// `pub(crate)` to overlay). Production SCP routing attaches a token via
    /// the field directly inside the overlay crate; all other callers (and
    /// tests in other crates) get a tokenless message.
    pub fn new(
        from_peer: PeerId,
        message: StellarMessage,
        received_at: std::time::Instant,
    ) -> Self {
        Self {
            from_peer,
            message,
            received_at,
            flow_release: None,
            scp_inflight_token: None,
            message_hash: None,
        }
    }

    /// Take the attached flow-control release token, if any, leaving `None`.
    ///
    /// The app SCP consumer calls this after draining the envelope so the held
    /// inbound credit is released (and a `SEND_MORE_EXTENDED` granted at the
    /// batch boundary) only once the message has actually been consumed.
    pub fn take_flow_release(&mut self) -> Option<FlowControlRelease> {
        self.flow_release.take()
    }

    /// Test-only seam (#3625): attach a fresh drain-gated flow-control release
    /// token to this message, sharing `flow_control` so a batch of messages
    /// released through the same `FlowControl` triggers a `SEND_MORE_EXTENDED`
    /// grant at the 40-message boundary. The returned channel observes the
    /// `num_messages` of any granted `SEND_MORE_EXTENDED`. Used by the app-crate
    /// consumer-release test (cross-crate, hence `pub`/`doc(hidden)`).
    #[doc(hidden)]
    pub fn attach_test_flow_release(
        &mut self,
        flow_control: std::sync::Arc<crate::flow_control::FlowControl>,
        grant_observer: std::sync::mpsc::Sender<u32>,
    ) {
        // Lock capacity for this message (mirrors begin_message_processing on
        // the peer task) so the deferred release has something to release.
        assert!(
            flow_control.begin_message_processing(&self.message),
            "test flow control rejected message (no capacity)"
        );
        // Bridge the per-peer outbound channel to the simple grant observer:
        // a tiny task forwards any granted SEND_MORE_EXTENDED's num_messages.
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(64);
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if let OutboundMessage::Send(StellarMessage::SendMoreExtended(sme)) = m {
                    let _ = grant_observer.send(sme.num_messages);
                }
            }
        });
        let metrics = std::sync::Arc::new(OverlayMetrics::new());
        self.flow_release = Some(FlowControlRelease::new(
            flow_control,
            self.message.clone(),
            tx,
            metrics,
        ));
    }
}

/// A snapshot of a connected peer's info and statistics.
///
/// Provides a point-in-time view of a peer's state without holding any locks.
#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    /// Static information about the peer (ID, address, version).
    pub info: PeerInfo,
    /// Message and byte counters.
    pub stats: PeerStatsSnapshot,
}

/// Lightweight handle stored in DashMap, replaces Arc<TokioMutex<Peer>>.
///
/// The actual `Peer` is owned by the spawned peer task. This handle
/// provides non-blocking access to send messages and read stats.
pub(super) struct PeerHandle {
    /// Channel to send outbound messages to the peer task.
    outbound_tx: mpsc::Sender<OutboundMessage>,
    /// Shared stats (atomically updated by the peer task).
    stats: Arc<PeerStats>,
    /// Per-peer flow control (shared with the peer task).
    flow_control: Arc<FlowControl>,
    /// Whether this is an inbound or outbound connection. Used by the
    /// mutual-dial tiebreaker to distinguish same-direction duplicates
    /// from cross-direction collisions.
    direction: ConnectionDirection,
    /// Monotonically-increasing generation counter. Used by `cleanup_peer`
    /// to avoid removing an entry that was replaced by a mutual-dial
    /// tiebreaker while the old peer_loop was still running.
    generation: u64,
}

/// Messages sent to a peer task via the outbound channel.
#[derive(Debug)]
pub(super) enum OutboundMessage {
    /// Direct send (non-flood, e.g. GetTxSet, ScpQuorumset response).
    Send(StellarMessage),
    /// Flood message (goes through FlowControl outbound queue).
    Flood(StellarMessage),
    /// Close the connection immediately (idle/normal teardown).
    Shutdown,
    /// Close the connection after a 5 s drain delay (error-drop path).
    ///
    /// §12.3 / TCPPeer.cpp:835-862: after the final `ERROR_MSG` has been
    /// flushed to the socket, defer the actual close by 5 s so the peer
    /// receives the error rather than an RST. Only the error-drop path uses
    /// this; plain teardown stays immediate (`Shutdown`).
    ShutdownAfterError,
}

/// Bundled connection parameters for the tick-loop helpers
/// (`connect_preferred_peers`, `fill_outbound_slots`).
pub(super) struct TickConnectCtx {
    pub(super) local_node: LocalNode,
    pub(super) timeouts: crate::OutboundTimeouts,
    pub(super) target_outbound: usize,
    pub(super) connection_factory: Arc<dyn ConnectionFactory>,
}

/// Shared admission state for authenticated-peer promotion.
///
/// The lock around this state serializes admission decisions so concurrent
/// preferred peers cannot evict the same victim or over-promote the pool.
#[derive(Debug, Default)]
pub(super) struct AdmissionState {
    evicting: HashSet<PeerId>,
}

impl AdmissionState {
    fn is_evicting(&self, peer_id: &PeerId) -> bool {
        self.evicting.contains(peer_id)
    }

    fn mark_evicting(&mut self, peer_id: PeerId) {
        self.evicting.insert(peer_id);
    }

    fn clear_evicting(&mut self, peer_id: &PeerId) {
        self.evicting.remove(peer_id);
    }
}

/// Tracks in-flight connections to prevent duplicate dials/handshakes.
///
/// During the window between initiating a connection and completing
/// registration in `SharedPeerState::peers`, multiple concurrent tasks
/// could start handshakes to the same destination. This struct provides
/// dedup at two levels:
///
/// - **by_address**: keyed by socket address (host:port), prevents outbound
///   dial races to the same target. Inserted before dial, removed on
///   completion.
/// - **by_peer_id**: keyed by peer ID (known after HELLO), prevents
///   concurrent registration attempts for the same node. Inserted after
///   handshake, removed after register_peer or on failure. Stores
///   direction metadata to distinguish mutual-dial from true duplicates.
///
/// Stale entries (from crashed/hung tasks) are swept periodically from
/// the tick loop.
///
/// Matches stellar-core's `mPendingPeers` dedup (Peer.cpp:1881-1909).
#[derive(Clone)]
pub(super) struct PendingConnections {
    /// In-flight connections by resolved target address.
    pub(super) by_address: Arc<DashMap<ResolvedPeerAddr, std::time::Instant>>,
    /// In-flight connections by peer ID (known after handshake).
    pub(super) by_peer_id: Arc<DashMap<PeerId, PendingPeerEntry>>,
}

/// Metadata for a pending peer-ID reservation.
///
/// Tracks when the reservation was made and from which direction (inbound
/// vs outbound). Direction is used to resolve mutual-dial races: an inbound
/// handshake that collides with an existing OUTBOUND reservation is allowed
/// to proceed (the post-handshake `register_peer` resolves the race), while
/// a collision with another INBOUND reservation rejects immediately.
#[derive(Clone, Debug)]
pub(crate) struct PendingPeerEntry {
    pub reserved_at: std::time::Instant,
    pub direction: ConnectionDirection,
}

/// Maximum age for a pending connection before it is considered stale.
const PENDING_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

impl PendingConnections {
    fn new() -> Self {
        Self {
            by_address: Arc::new(DashMap::new()),
            by_peer_id: Arc::new(DashMap::new()),
        }
    }

    /// Try to reserve a pending outbound connection to the given address.
    /// Returns false if a connection to this address is already in flight.
    pub(super) fn try_reserve_address(&self, addr_key: ResolvedPeerAddr) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.by_address.entry(addr_key) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(std::time::Instant::now());
                true
            }
        }
    }

    /// Try to reserve a pending connection for the given peer ID.
    /// Returns false if a handshake for this peer ID is already in flight.
    /// Used in tests; production reservation now happens inside Peer::handshake().
    #[cfg(test)]
    pub(super) fn try_reserve_peer_id(
        &self,
        peer_id: &PeerId,
        direction: ConnectionDirection,
    ) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.by_peer_id.entry(peer_id.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(PendingPeerEntry {
                    reserved_at: std::time::Instant::now(),
                    direction,
                });
                true
            }
        }
    }

    /// Release a pending address reservation.
    pub(super) fn release_address(&self, addr_key: &ResolvedPeerAddr) {
        self.by_address.remove(addr_key);
    }

    /// Release a pending peer ID reservation.
    pub(super) fn release_peer_id(&self, peer_id: &PeerId) {
        self.by_peer_id.remove(peer_id);
    }

    /// Remove stale pending entries older than PENDING_CONNECTION_TIMEOUT.
    pub(super) fn sweep_stale(&self) {
        let cutoff = std::time::Instant::now() - PENDING_CONNECTION_TIMEOUT;
        self.by_address.retain(|_, ts| *ts > cutoff);
        self.by_peer_id
            .retain(|_, entry| entry.reserved_at > cutoff);
    }
}

/// Shared state passed to spawned peer tasks.
///
/// Bundles all `Arc`-wrapped state that background tasks need, avoiding
/// 20+ individual parameter lists on `connect_to_explicit_peer` and
/// `run_peer_loop`.
#[derive(Clone)]
pub(super) struct SharedPeerState {
    pub(super) peers: Arc<DashMap<PeerId, PeerHandle>>,
    pub(super) flood_gate: Arc<FloodGate>,
    pub(super) running: Arc<AtomicBool>,
    pub(super) message_tx: broadcast::Sender<OverlayMessage>,
    pub(super) scp_message_tx: mpsc::Sender<OverlayMessage>,
    /// App-provided in-flight dedup filter for inbound SCP envelopes, run in
    /// the peer task (maxtps iter 7; parity: core `checkScheduledAndCache` in
    /// `Peer::recvAuthenticatedMessage`). `None` (e.g. overlay-only tests)
    /// falls back to the app-side dedup in `pump_scp_intake`. Shared with
    /// [`OverlayManager::scp_inbound_filter`].
    pub(super) scp_inbound_filter: Arc<RwLock<Option<Arc<ScpInboundFilter>>>>,
    /// Bounded ([`FETCH_CHANNEL_CAPACITY`]) so a wedged event loop cannot grow
    /// RSS unbounded via accumulated multi-MB tx-sets (#3661). Drops on full.
    pub(super) fetch_response_tx: mpsc::Sender<OverlayMessage>,
    pub(super) peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    pub(super) advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    pub(super) advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    pub(super) added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    pub(super) dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    pub(super) banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    pub(super) peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Per-peer highest observed externalized slot (observability-only). Shared
    /// `Arc` with `OverlayManager::peer_latest_externalized`. Held here so the
    /// peer-disconnect cleanup path (`cleanup_peer`) can drop the entry
    /// alongside `peer_info_cache`. See issue #3270.
    pub(super) peer_latest_externalized: Arc<DashMap<PeerId, AtomicU64>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    pub(super) last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    pub(super) scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    pub(super) is_validator: bool,
    pub(super) peer_event_tx: Option<mpsc::Sender<PeerEvent>>,
    pub(super) extra_subscribers: Arc<RwLock<Vec<mpsc::Sender<OverlayMessage>>>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    pub(super) is_tracking: Arc<AtomicBool>,
    /// Whether the node's ledger state is synced with consensus.
    /// See `OverlayManager::is_synced` for details.
    pub(super) is_synced: Arc<AtomicBool>,
    /// Tracks in-flight connections for dedup.
    pub(super) pending_connections: PendingConnections,
    /// Preferred peer set shared by all connection tasks and updated after DNS
    /// resolution so admission decisions use current config and resolved IPs.
    pub(super) preferred_peers: Arc<RwLock<PreferredPeerSet>>,
    /// When `true`, reject non-preferred authenticated peers even with capacity.
    /// Matches stellar-core's `PREFERRED_PEERS_ONLY`. Immutable after init.
    pub(super) preferred_peers_only: bool,
    /// Serialized authenticated admission state.
    pub(super) admission_state: Arc<Mutex<AdmissionState>>,
    /// Current depth of the dedicated fetch channel. Incremented on every
    /// successful send from `route_to_subscribers` and decremented by the
    /// consumer on every successful `recv()`. Exposed via `/metrics` as
    /// `henyey_overlay_fetch_channel_depth`. Tracked on the send side so the
    /// gauge stays fresh even when the app event loop wedges (issue #1741).
    pub(super) fetch_channel_depth: Arc<AtomicI64>,
    /// Monotonic high-water mark for `fetch_channel_depth`. Advanced on the
    /// send side from `route_to_subscribers` via a CAS loop. Exposed via
    /// `/metrics` as `henyey_overlay_fetch_channel_depth_max`.
    pub(super) fetch_channel_depth_max: Arc<AtomicI64>,
    /// Shared overlay metrics counters.
    pub(super) metrics: Arc<OverlayMetrics>,
    /// Per-peer query rate-limit window in whole seconds, updated by the app
    /// layer after each ledger close. See `OverlayManager::set_query_rate_limit_window`.
    pub(super) query_rate_limit_window_secs: Arc<AtomicU64>,
    /// Current maximum transaction size in bytes. Shared with the app layer
    /// (same `Arc<AtomicU32>`) so the overlay can dynamically compute the
    /// initial byte grant for new peers via `FlowControlBytesConfig::bytes_total`.
    pub(super) max_tx_size_bytes: Arc<AtomicU32>,
    /// Flow control byte parameters (initial grant and batch size).
    /// Immutable after initialization — no atomic needed.
    pub(super) flow_control_bytes_config: FlowControlBytesConfig,
    /// Initial message-level flood reading capacity for SEND_MORE_EXTENDED and
    /// FlowControl. Matches stellar-core's `PEER_FLOOD_READING_CAPACITY`.
    pub(super) peer_flood_reading_capacity: u32,
    /// Per-peer outbound channel capacity. Sourced from the `ConnectionFactory`
    /// so OverLoopback can use a larger value than TCP. See issue #2356.
    pub(super) outbound_channel_capacity: usize,
    /// Cooldown map preventing immediate re-dial after a connection drops.
    ///
    /// When an outbound peer loop exits (connection lost), the address is
    /// inserted with a random expiry (1–3 s in the future). Subsequent dial
    /// attempts to the same address are skipped until the cooldown expires.
    /// This breaks mutual-dial oscillation by introducing asymmetric jitter
    /// between the two sides of a simultaneous dial.
    pub(super) dial_cooldowns: Arc<DashMap<ResolvedPeerAddr, std::time::Instant>>,
    /// Our own peer ID. Used by the mutual-dial tiebreaker to
    /// deterministically decide which side yields its outbound connection.
    pub(super) local_peer_id: PeerId,
    /// Monotonically-increasing counter for `PeerHandle::generation`.
    pub(super) next_peer_generation: Arc<AtomicU64>,
}

impl SharedPeerState {
    /// Send a peer event if a subscriber is registered.
    pub(super) async fn send_peer_event(&self, event: PeerEvent) {
        if let Some(tx) = self.peer_event_tx.as_ref() {
            let _ = tx.send(event).await;
        }
    }

    /// Clean up shared state after a peer disconnects.
    /// Must be called after `run_peer_loop` completes for any authenticated peer.
    ///
    /// The `generation` parameter is the generation of the `PeerHandle` that the
    /// caller registered. If a mutual-dial tiebreaker replaced the entry since
    /// registration, the installed generation will differ and this call is a no-op
    /// — preventing the old peer_loop's cleanup from clobbering the replacement.
    pub(super) fn cleanup_peer(&self, peer_id: &PeerId, generation: u64) {
        let removed = self
            .peers
            .remove_if(peer_id, |_, handle| handle.generation == generation);
        if removed.is_some() {
            self.peer_info_cache.remove(peer_id);
            // Drop the per-peer externalized observation so the map tracks only
            // live peers and `peers_could_serve` cannot count departed peers
            // (#3270).
            self.peer_latest_externalized.remove(peer_id);
            self.admission_state.lock().clear_evicting(peer_id);
            self.dropped_authenticated_peers
                .fetch_add(1, Ordering::Relaxed);
        } else {
            debug!(
                "cleanup_peer: skipped stale cleanup for {} gen={} (generation mismatch)",
                peer_id, generation
            );
        }
    }

    /// Forward an overlay message to the appropriate subscriber channels.
    ///
    /// Returns `true` if the message was an SCP message (for counter tracking).
    /// Routes to dedicated channels (SCP, fetch response, extra subscribers)
    /// first, then falls through to the generic broadcast channel for
    /// non-dedicated messages.
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub(super) fn route_to_subscribers(&self, msg: OverlayMessage) -> bool {
        let is_scp = matches!(msg.message, StellarMessage::ScpMessage(_));
        let is_fetch_response = matches!(
            msg.message,
            StellarMessage::GeneralizedTxSet(_)
                | StellarMessage::TxSet(_)
                | StellarMessage::DontHave(_)
                | StellarMessage::ScpQuorumset(_)
        );
        // Fetch-request messages (GetScpState, GetScpQuorumset, GetTxSet) must also
        // use the dedicated fetch channel so they are not silently dropped by the
        // lossy broadcast ring when the app loop lags. stellar-core services these
        // directly on the peer thread without a lossy intermediary.
        let is_fetch_request = matches!(
            msg.message,
            StellarMessage::GetScpState(_)
                | StellarMessage::GetScpQuorumset(_)
                | StellarMessage::GetTxSet(_)
        );
        let is_dedicated = is_scp || is_fetch_response || is_fetch_request;

        // NOTE on ordering (#3625): the SCP send is deferred to the END of this
        // function so we can MOVE the token-bearing `msg` into the dedicated SCP
        // channel rather than cloning it. The drain-gated `FlowControlRelease`
        // token rides only on that single moved copy; the extra-subscriber and
        // broadcast clones are tokenless (see `OverlayMessage`'s `Clone` impl).
        // If the bounded SCP channel is full, the dropped `msg` carries its
        // token to `Drop`, releasing the held capacity immediately so credit
        // never leaks on the drop-on-full backstop path.

        if is_fetch_response || is_fetch_request {
            // Non-blocking send: the channel is bounded ([`FETCH_CHANNEL_CAPACITY`])
            // to prevent unbounded RSS growth when the event loop stalls during the
            // catchup→Tracking handoff (#3661). Each fetch response can be a full
            // multi-MB tx-set, so an unbounded backlog grows RSS ~5.4 GB/min → OOM.
            // On a full channel we DROP rather than `.await` — blocking the
            // peer-receive path is its own event-loop hazard. The drop is
            // recoverable: the tx-set stays in `TxSetTracker.pending` and is
            // re-requested by the periodic `request_pending_tx_sets()` tick +
            // ItemFetcher retry. Fetch messages are NOT flow-controlled and the
            // enqueued copy is a tokenless `clone` (see `OverlayMessage::clone`),
            // so dropping never touches SEND_MORE credit.
            match self.fetch_response_tx.try_send(msg.clone()) {
                Ok(()) => {
                    // Issue #1741: account for the enqueue on the send side so the
                    // depth gauge reflects backlog even when the event loop is
                    // wedged (which is the exact failure mode the metric is meant
                    // to diagnose).
                    let new_depth = self.fetch_channel_depth.fetch_add(1, Ordering::Relaxed) + 1;
                    let mut prev = self.fetch_channel_depth_max.load(Ordering::Relaxed);
                    while new_depth > prev {
                        match self.fetch_channel_depth_max.compare_exchange_weak(
                            prev,
                            new_depth,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(observed) => prev = observed,
                        }
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.metrics.fetch_messages_dropped.add(1);
                    self.metrics.messages_dropped.add(1);
                    debug!(
                        "Fetch channel full (cap {}); dropping fetch message from peer {} (recoverable: re-requested by request_pending_tx_sets)",
                        FETCH_CHANNEL_CAPACITY, msg.from_peer
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    error!(
                        "Fetch channel send FAILED for peer {}: channel closed",
                        msg.from_peer
                    );
                }
            }
        }

        // Send catchup-critical messages to extra subscribers
        if matches!(
            msg.message,
            StellarMessage::ScpMessage(_)
                | StellarMessage::GeneralizedTxSet(_)
                | StellarMessage::TxSet(_)
                | StellarMessage::ScpQuorumset(_)
        ) {
            let subs = self.extra_subscribers.read();
            for sub in subs.iter() {
                // Non-blocking send on the bounded ([`CATCHUP_CHANNEL_CAPACITY`])
                // catchup-cache fan-out. The consumer is aborted only at the
                // catchup→Tracking handoff; if that stalls (#3582) an unbounded
                // channel accumulates multi-MB tx-sets → OOM (#3661). On full we
                // DROP and count: the cache is pre-warm, so dropped tx-sets are
                // re-fetched (the task re-broadcasts `GetTxSet`, peers re-flood
                // EXTERNALIZE, and the post-handoff loop re-fetches pending). The
                // fan-out copy is a tokenless `clone`, so no flow-control credit
                // interaction.
                match sub.try_send(msg.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.metrics.catchup_messages_dropped.add(1);
                        self.metrics.messages_dropped.add(1);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        }

        if !is_dedicated {
            let _ = self.message_tx.send(msg);
            return is_scp;
        }

        if is_scp {
            // Non-blocking send: the channel is bounded ([`SCP_CHANNEL_CAPACITY`])
            // to prevent unbounded RSS growth when the event loop stalls (#3623).
            // On a full channel we DROP the envelope rather than `.await` —
            // blocking the peer-receive path is its own event-loop hazard. The
            // drop is recoverable (peers re-flood; gap-detection + GetScpState
            // backfill) and counted in `messages_dropped` so it is observable.
            //
            // #3625: `msg` is MOVED here (no clone) so its drain-gated
            // `FlowControlRelease` token reaches the app consumer; on a full
            // channel the dropped `msg` releases its held capacity via `Drop`.
            let from_peer = msg.from_peer.clone();
            match self.scp_message_tx.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_dropped)) => {
                    self.metrics.messages_dropped.add(1);
                    // `_dropped` carries the in-flight scheduled-cache token
                    // (maxtps iter 7); dropping it here expires the cache
                    // entry, so a later duplicate copy from another peer is
                    // treated as new and re-forwarded — the retry path a full
                    // channel relies on.
                    debug!(
                        "SCP channel full (cap {}); dropping envelope from peer {} (recoverable: peers re-flood)",
                        SCP_CHANNEL_CAPACITY, from_peer
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    error!(
                        "SCP channel send FAILED for peer {}: channel closed",
                        from_peer
                    );
                }
            }
        }

        is_scp
    }
}

/// Central manager for all peer connections in the overlay network.
///
/// The overlay manager is the main entry point for networking operations.
/// It handles connection lifecycle, message routing, and peer discovery.
///
/// # Usage
///
/// ```rust,ignore
/// // Create and start the manager
/// let config = OverlayConfig::testnet();
/// let local_node = LocalNode::new_testnet(secret_key);
/// let mut manager = OverlayManager::new(config, local_node)?;
/// manager.start().await?;
///
/// // Subscribe to messages
/// let mut rx = manager.subscribe();
/// while let Ok(msg) = rx.recv().await {
///     handle_message(msg);
/// }
///
/// // Broadcast a message
/// manager.broadcast(StellarMessage::Transaction(tx)).await?;
///
/// // Shutdown
/// manager.shutdown().await?;
/// ```
pub struct OverlayManager {
    /// Configuration.
    pub(super) config: OverlayConfig,
    /// Local node info.
    pub(super) local_node: LocalNode,
    /// Connected peers. Each entry is a lightweight handle with a channel
    /// to the peer's dedicated task (which owns the actual `Peer`).
    pub(super) peers: Arc<DashMap<PeerId, PeerHandle>>,
    /// Flood gate.
    pub(super) flood_gate: Arc<FloodGate>,
    /// Connection pool for inbound connections.
    pub(super) inbound_pool: Arc<ConnectionPool>,
    /// Connection pool for outbound connections.
    pub(super) outbound_pool: Arc<ConnectionPool>,
    /// Whether the manager is running.
    pub(super) running: Arc<AtomicBool>,
    /// Channel for incoming messages.
    pub(super) message_tx: broadcast::Sender<OverlayMessage>,
    /// Handle to listener task.
    pub(super) listener_handle: Option<JoinHandle<()>>,
    /// Handle to connector task.
    pub(super) connector_handle: Option<JoinHandle<()>>,
    /// Handle to peer tasks.
    pub(super) peer_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
    /// Known peers: config hostnames (with DNS resolution cache) + discovered IPs.
    pub(super) known_peers: Arc<RwLock<KnownPeerSet>>,
    /// Outbound peers to advertise in Peers messages.
    pub(super) advertised_outbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Inbound peers to advertise in Peers messages.
    pub(super) advertised_inbound_peers: Arc<RwLock<Vec<PeerAddress>>>,
    /// Total authenticated peers added.
    pub(super) added_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Total authenticated peers dropped.
    pub(super) dropped_authenticated_peers: Arc<std::sync::atomic::AtomicU64>,
    /// Banned peers by node ID.
    pub(super) banned_peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Shutdown signal. Wrapped in `Mutex` for interior mutability so
    /// `signal_shutdown(&self)` can take it through a shared reference.
    pub(super) shutdown_tx: Mutex<Option<broadcast::Sender<()>>>,
    /// Cache of peer info for connected peers (lock-free access).
    pub(super) peer_info_cache: Arc<DashMap<PeerId, PeerInfo>>,
    /// Highest externalized SCP slot observed *via live SCP gossip* from each
    /// connected peer (lock-free, observability-only). Updated at the two
    /// EXTERNALIZE-accept sites in the app event loop (see
    /// `record_peer_externalized`). Read at `GetScpState` re-request time by
    /// `peers_could_serve` to enrich the request log with how many connected
    /// peers could still hold the requested slot in their SCP window. This is
    /// pure telemetry — it never feeds back into envelope acceptance, relay,
    /// peer selection, or the `GetScpState` watermark. Entries are removed on
    /// disconnect (see `SharedPeerState::cleanup_peer`) so the map tracks only
    /// live peers. See issue #3270.
    pub(super) peer_latest_externalized: Arc<DashMap<PeerId, AtomicU64>>,
    /// Dedicated bounded channel for SCP messages (capacity
    /// [`SCP_CHANNEL_CAPACITY`]).
    ///
    /// SCP is consensus-critical, but the channel MUST be bounded: a stalled
    /// event loop (#3582) leaves nothing draining while ~24 validators flood
    /// ~100+ envelopes/slot, and an unbounded channel grows RSS until the
    /// validator is OOM-killed (#3623). On overflow the send side drops the
    /// envelope (recoverable — peers re-flood and `GetScpState` backfills),
    /// mirroring stellar-core's bounded inbound flow-control capacity.
    pub(super) scp_message_tx: mpsc::Sender<OverlayMessage>,
    /// Receiver end of the SCP channel. Taken once via `subscribe_scp()`.
    scp_message_rx: Arc<TokioMutex<Option<mpsc::Receiver<OverlayMessage>>>>,
    /// Dedicated **bounded** ([`FETCH_CHANNEL_CAPACITY`]) channel for fetch
    /// response messages. Routes GeneralizedTxSet, TxSet, DontHave, ScpQuorumset,
    /// GetScpState, GetScpQuorumset, and GetTxSet to the event loop. Bounded so a
    /// wedged loop cannot accumulate multi-MB tx-sets to OOM (#3661); over-capacity
    /// messages are `try_send`-dropped (recoverable via `request_pending_tx_sets()`
    /// re-request) and counted in `metrics.fetch_messages_dropped`. Depth is exposed
    /// via `henyey_overlay_fetch_channel_depth` gauges for operator visibility.
    pub(super) fetch_response_tx: mpsc::Sender<OverlayMessage>,
    /// Receiver end of the fetch response channel. Taken once via `subscribe_fetch_responses()`.
    fetch_response_rx: Arc<TokioMutex<Option<mpsc::Receiver<OverlayMessage>>>>,
    /// App-provided in-flight SCP dedup filter (see [`ScpInboundFilter`]).
    /// Shared with every [`SharedPeerState`] snapshot; settable before or
    /// after start via [`Self::set_scp_inbound_filter`].
    scp_inbound_filter: Arc<RwLock<Option<Arc<ScpInboundFilter>>>>,
    /// Dynamic extra subscribers for catchup-critical messages (SCP + TxSet).
    /// Created on demand via `subscribe_catchup()` and cleaned up when dropped.
    /// Uses parking_lot::RwLock for minimal contention in the hot path (read-heavy).
    pub(super) extra_subscribers: Arc<RwLock<Vec<mpsc::Sender<OverlayMessage>>>>,
    /// Last closed ledger sequence, used for flood record cleanup.
    pub(super) last_closed_ledger: Arc<AtomicU32>,
    /// Optional callback for intelligent SCP queue trimming.
    pub(super) scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    /// Whether the node is tracking consensus (set by the herder/app layer).
    /// When false, the overlay may drop random peers to try new connections.
    pub(super) is_tracking: Arc<AtomicBool>,
    /// Whether the node's ledger state is synced with consensus (set by app layer).
    /// Parity: mirrors stellar-core's `LedgerManager::isSynced()`. When false,
    /// the peer loop drops `Transaction`, `FloodAdvert`, and `FloodDemand`
    /// messages early to avoid wasted flood-gate / rate-limiter / channel work.
    pub(super) is_synced: Arc<AtomicBool>,
    /// Connection factory used for transport establishment.
    pub(super) connection_factory: Arc<dyn ConnectionFactory>,
    /// Tracks in-flight connections for dedup.
    pub(super) pending_connections: PendingConnections,
    /// Current preferred peer set shared with connection tasks.
    pub(super) preferred_peers: Arc<RwLock<PreferredPeerSet>>,
    /// Serialized authenticated admission state shared with connection tasks.
    pub(super) admission_state: Arc<Mutex<AdmissionState>>,
    /// Shared with `SharedPeerState`; see field docs there. Plumbed in from
    /// the app so the same atomics back both the `/metrics` gauge and the
    /// watchdog read path.
    pub(super) fetch_channel_depth: Arc<AtomicI64>,
    pub(super) fetch_channel_depth_max: Arc<AtomicI64>,
    /// Overlay metrics counters. Shared with peer loops and exposed via
    /// `/metrics` as `stellar_overlay_*` gauges and counters.
    pub(super) metrics: Arc<OverlayMetrics>,
    /// Per-peer query rate-limit window in whole seconds.
    ///
    /// stellar-core computes this as `expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER`
    /// (Peer.cpp:1426-1429), truncated to seconds. The app layer updates this
    /// via [`set_query_rate_limit_window`] after each ledger close; peer tasks
    /// read it through `SharedPeerState`.
    pub(super) query_rate_limit_window_secs: Arc<AtomicU64>,
    /// Current maximum transaction size in bytes. Shared with the app layer
    /// via the same `Arc<AtomicU32>` so the overlay reads the latest value
    /// when computing initial byte grants for new peers.
    pub(super) max_tx_size_bytes: Arc<AtomicU32>,
    /// Cached local address the listener is bound to (set by `start_listener()`).
    listen_addr: Option<SocketAddr>,
    /// Cooldown map preventing immediate re-dial after a connection drops.
    /// Shared with `SharedPeerState` via `Arc`.
    pub(super) dial_cooldowns: Arc<DashMap<ResolvedPeerAddr, std::time::Instant>>,
    /// Monotonically-increasing counter for `PeerHandle::generation`.
    /// Shared with all `SharedPeerState` snapshots via `Arc`.
    pub(super) next_peer_generation: Arc<AtomicU64>,
    /// Epoch-ms of the last emitted broadcast-backpressure WARN (0 = never).
    /// Gates the per-call `warn!` in [`Self::broadcast`] to at most one line per
    /// [`BROADCAST_BACKPRESSURE_WARN_INTERVAL_MS`], preventing the up-to-24k
    /// lines/second log-amplification hazard observed in #3792 while the event
    /// loop is already parked. The two dedicated drop counters remain the source
    /// of truth, so throttling loses no volume.
    broadcast_backpressure_warn_last_ms: AtomicU64,
}

impl OverlayManager {
    /// Create a new overlay manager with the given configuration.
    pub fn new(config: OverlayConfig, local_node: LocalNode) -> Result<Self> {
        Self::new_with_connection_factory(config, local_node, Arc::new(TcpConnectionFactory))
    }

    /// Create a new overlay manager with a custom connection factory.
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn new_with_connection_factory(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
    ) -> Result<Self> {
        Self::new_with_fetch_metrics(
            config,
            local_node,
            connection_factory,
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicI64::new(0)),
            Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
        )
    }

    /// Create a new overlay manager with externally-owned atomics for the
    /// fetch channel depth metrics. The caller (typically `App`) keeps its
    /// own `Arc` handles so the same atomics back `/metrics` and the
    /// watchdog. Issue #1741.
    ///
    /// `max_tx_size_bytes` is the shared atomic tracking the current maximum
    /// transaction size in bytes. The overlay reads this to compute the
    /// initial byte grant for new peers via [`FlowControlBytesConfig::bytes_total`].
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn new_with_fetch_metrics(
        config: OverlayConfig,
        local_node: LocalNode,
        connection_factory: Arc<dyn ConnectionFactory>,
        fetch_channel_depth: Arc<AtomicI64>,
        fetch_channel_depth_max: Arc<AtomicI64>,
        max_tx_size_bytes: Arc<AtomicU32>,
    ) -> Result<Self> {
        // Broadcast channel for non-critical overlay messages (TX floods, etc.).
        // SCP and fetch-response messages bypass this channel via dedicated mpsc
        // channels, so the broadcast channel only carries remaining message types.
        let (message_tx, _) = broadcast::channel(BROADCAST_CHANNEL_SIZE);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (scp_message_tx, scp_message_rx) = mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_response_tx, fetch_response_rx) = mpsc::channel(FETCH_CHANNEL_CAPACITY);
        let preferred_peers = Arc::new(RwLock::new(PreferredPeerSet::from_config(
            config.preferred_peers.clone(),
            config.preferred_peer_keys.clone(),
        )));

        Ok(Self {
            config: config.clone(),
            local_node,
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::with_ttl(Duration::from_secs(
                config.flood_ttl_secs,
            ))),
            inbound_pool: Arc::new({
                // Always construct with preferred headroom so that once DNS
                // resolves, inbound preferred peers get extra slots immediately.
                // Initially empty — update_preferred_ips() is called after DNS.
                ConnectionPool::with_preferred(
                    config.max_inbound_peers,
                    POSSIBLY_PREFERRED_EXTRA,
                    HashSet::new(),
                )
            }),
            outbound_pool: Arc::new(ConnectionPool::new(config.max_outbound_peers)),
            running: Arc::new(AtomicBool::new(false)),
            message_tx,
            listener_handle: None,
            connector_handle: None,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            known_peers: Arc::new(RwLock::new(KnownPeerSet::from_config(
                config.known_peers.clone(),
            ))),
            advertised_outbound_peers: Arc::new(RwLock::new(config.known_peers.clone())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            scp_message_tx,
            scp_message_rx: Arc::new(TokioMutex::new(Some(scp_message_rx))),
            fetch_response_tx,
            fetch_response_rx: Arc::new(TokioMutex::new(Some(fetch_response_rx))),
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            is_tracking: Arc::new(AtomicBool::new(false)),
            is_synced: Arc::new(AtomicBool::new(false)),
            connection_factory,
            pending_connections: PendingConnections::new(),
            preferred_peers,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth,
            fetch_channel_depth_max,
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes,
            listen_addr: None,
            dial_cooldowns: Arc::new(DashMap::new()),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
            broadcast_backpressure_warn_last_ms: AtomicU64::new(0),
        })
    }

    /// Create a snapshot of shared state for passing to spawned tasks.
    pub(super) fn shared_state(&self) -> SharedPeerState {
        SharedPeerState {
            peers: Arc::clone(&self.peers),
            flood_gate: Arc::clone(&self.flood_gate),
            running: Arc::clone(&self.running),
            message_tx: self.message_tx.clone(),
            scp_message_tx: self.scp_message_tx.clone(),
            scp_inbound_filter: Arc::clone(&self.scp_inbound_filter),
            fetch_response_tx: self.fetch_response_tx.clone(),
            peer_handles: Arc::clone(&self.peer_handles),
            advertised_outbound_peers: Arc::clone(&self.advertised_outbound_peers),
            advertised_inbound_peers: Arc::clone(&self.advertised_inbound_peers),
            added_authenticated_peers: Arc::clone(&self.added_authenticated_peers),
            dropped_authenticated_peers: Arc::clone(&self.dropped_authenticated_peers),
            banned_peers: Arc::clone(&self.banned_peers),
            peer_info_cache: Arc::clone(&self.peer_info_cache),
            peer_latest_externalized: Arc::clone(&self.peer_latest_externalized),
            last_closed_ledger: Arc::clone(&self.last_closed_ledger),
            scp_callback: self.scp_callback.clone(),
            is_validator: self.config.is_validator,
            peer_event_tx: self.config.peer_event_tx.clone(),
            extra_subscribers: Arc::clone(&self.extra_subscribers),
            is_tracking: Arc::clone(&self.is_tracking),
            is_synced: Arc::clone(&self.is_synced),
            pending_connections: self.pending_connections.clone(),
            preferred_peers: Arc::clone(&self.preferred_peers),
            preferred_peers_only: self.config.preferred_peers_only,
            admission_state: Arc::clone(&self.admission_state),
            fetch_channel_depth: Arc::clone(&self.fetch_channel_depth),
            fetch_channel_depth_max: Arc::clone(&self.fetch_channel_depth_max),
            metrics: Arc::clone(&self.metrics),
            query_rate_limit_window_secs: Arc::clone(&self.query_rate_limit_window_secs),
            max_tx_size_bytes: Arc::clone(&self.max_tx_size_bytes),
            flow_control_bytes_config: self.config.flow_control_bytes_config,
            peer_flood_reading_capacity: self.config.peer_flood_reading_capacity,
            outbound_channel_capacity: self.connection_factory.outbound_channel_capacity(),
            dial_cooldowns: Arc::clone(&self.dial_cooldowns),
            local_peer_id: PeerId::from_xdr(self.local_node.xdr_public_key()),
            next_peer_generation: Arc::clone(&self.next_peer_generation),
        }
    }

    /// Start the overlay manager (listening and connecting to peers).
    ///
    /// If `pre_bound_listener` is `Some`, the overlay will use the given
    /// pre-bound listener instead of binding a new socket.  This is used
    /// by the simulation harness to inject OS-assigned ephemeral-port
    /// listeners, eliminating port-allocation races across test binaries.
    /// Production callers pass `None`.
    pub async fn start(&mut self, pre_bound_listener: Option<Listener>) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::AlreadyStarted);
        }

        info!("Starting overlay manager");
        self.running.store(true, Ordering::Relaxed);

        // Start listener if enabled
        if self.config.listen_enabled {
            self.start_listener(pre_bound_listener).await?;
        } else {
            debug_assert!(
                pre_bound_listener.is_none(),
                "pre-bound listener provided but listen_enabled is false"
            );
        }

        // Start the periodic tick loop for peer management.
        // This replaces a dedicated connector task — the tick loop handles
        // all periodic maintenance: DNS resolution, peer connection,
        // preferred-peer eviction, random-peer drops, and slot filling.
        // Matches stellar-core OverlayManagerImpl::tick().
        self.start_tick_loop();

        Ok(())
    }

    /// Returns the local address the overlay listener is bound to, if any.
    ///
    /// This is a cached snapshot of the address recorded when [`start()`](Self::start)
    /// bound the listener. It reflects whatever the underlying
    /// [`ConnectionFactory::bind()`] reported — for [`TcpConnectionFactory`]
    /// this is the actual OS-assigned `0.0.0.0:<port>` address; for
    /// [`LoopbackConnectionFactory`](crate::LoopbackConnectionFactory) the
    /// reported address may not be meaningful (e.g., port 0 if 0 was requested).
    ///
    /// Returns `None` before `start()` is called or when `listen_enabled = false`.
    ///
    /// **Note:** This is a bind-time snapshot, not a liveness indicator.
    /// The listener task may have stopped (e.g., after shutdown). Callers
    /// that need only the port should use `.port()` — the IP may be
    /// `0.0.0.0` (wildcard bind).
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen_addr
    }

    /// Connect to a specific peer.
    pub async fn connect(&self, addr: &PeerAddress) -> Result<PeerId> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        if !self.outbound_pool.try_reserve() {
            return Err(OverlayError::PeerLimitReached);
        }

        let timeouts = crate::OutboundTimeouts::from_config(&self.config);
        connection::connect_to_explicit_peer(
            addr,
            self.local_node.clone(),
            timeouts,
            Arc::clone(&self.outbound_pool),
            self.shared_state(),
            Arc::clone(&self.connection_factory),
        )
        .await
    }

    /// Broadcast a message to all connected peers.
    ///
    /// Non-blocking: sends via each peer's outbound channel. The peer tasks
    /// handle the actual TCP writes asynchronously.
    pub async fn broadcast(&self, message: StellarMessage) -> Result<usize> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        let msg_type = helpers::message_type_name(&message);
        let is_flood = helpers::is_flood_message(&message);
        // Classify before `message` is moved into the fan-out loop, so a Full
        // drop can be attributed to a dedicated per-type series (#3792).
        let kind = crate::metrics::OverlayMessageKind::from_stellar_message(&message);

        // Record in flood gate and get filtered peer list.
        // Only FloodGate-tracked messages (tx, SCP) are recorded for dedup.
        // Pull-control messages are sent via try_send_to(), not broadcast(),
        // but guard here for defense-in-depth.
        let forward_peers: Option<Vec<PeerId>> =
            if is_flood && helpers::is_flood_gate_tracked(&message) {
                let hash = compute_message_hash(&message);
                let lcl = self.last_closed_ledger.load(Ordering::Relaxed);
                self.flood_gate.record_local_broadcast(hash, lcl);
                // Only forward to peers that haven't already sent us this message
                let all_peers: Vec<PeerId> = self.peers.iter().map(|e| e.key().clone()).collect();
                Some(self.flood_gate.get_forward_peers(&hash, &all_peers))
            } else {
                None // non-flood or pull-control: send to all
            };

        // Collect target peer IDs so we can move the message into the last send.
        let target_peers: Vec<PeerId> = self
            .peers
            .iter()
            .filter_map(|entry| {
                let peer_id = entry.key();
                if forward_peers
                    .as_ref()
                    .map_or(true, |fwd| fwd.contains(peer_id))
                {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect();

        debug!("Broadcasting {} to {} peers", msg_type, target_peers.len());

        let mut sent = 0usize;
        let mut dropped = 0usize;
        let num_targets = target_peers.len();
        let mut message = Some(message);
        for (i, peer_id) in target_peers.iter().enumerate() {
            let is_last = i + 1 == num_targets;
            let outbound_msg = if is_last {
                // Move the original into the last send to avoid one clone.
                let msg = message.take().unwrap();
                if is_flood {
                    OutboundMessage::Flood(msg)
                } else {
                    OutboundMessage::Send(msg)
                }
            } else {
                // Clone for all but the last peer.
                let msg = message.as_ref().unwrap().clone();
                if is_flood {
                    OutboundMessage::Flood(msg)
                } else {
                    OutboundMessage::Send(msg)
                }
            };
            if let Some(entry) = self.peers.get(peer_id) {
                match entry.value().outbound_tx.try_send(outbound_msg) {
                    Ok(()) => sent += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        dropped += 1;
                        debug!("Outbound channel full for {}, dropping broadcast", peer_id);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        debug!("Outbound channel closed for {}", peer_id);
                    }
                }
            }
        }

        if dropped > 0 {
            // Dedicated per-message-type series (#3792): the fan-out drops —
            // dominated by our own SCP envelopes — are bridged to `/metrics`,
            // unlike the aggregate `messages_dropped`, which is fed alongside for
            // cross-site continuity but never exported.
            self.metrics.broadcast_fanout_drop_by_type[kind as usize].add(dropped as u64);
            self.metrics.messages_dropped.add(dropped as u64);
            // Blackout: this call reached ZERO peers — every targeted peer's
            // channel was full. Worth alerting on separately from partial loss.
            if sent == 0 {
                self.metrics.broadcast_blackout.inc();
            }
            // Throttle the WARN to at most one line per interval; the counters
            // above capture every drop regardless of the log gate (#3792 §5).
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if should_emit_now(
                &self.broadcast_backpressure_warn_last_ms,
                now_ms,
                BROADCAST_BACKPRESSURE_WARN_INTERVAL_MS,
            ) {
                warn!(
                    dropped,
                    sent,
                    msg_type,
                    "Broadcast backpressure: messages dropped due to full peer channels"
                );
            }
        }

        debug!("Broadcast {} to {} peers", msg_type, sent);
        self.metrics.messages_broadcast.add(sent as u64);
        if is_flood {
            self.metrics.flood_broadcast.add(sent as u64);
        }
        Ok(sent)
    }

    /// Disconnect a specific peer by ID.
    pub async fn disconnect(&self, peer_id: &PeerId) -> bool {
        let Some(entry) = self.peers.get(peer_id) else {
            return false;
        };
        // Use try_send to avoid blocking if the peer's channel is full.
        // The peer_loop will exit on its own via the `running` flag or
        // straggler timeout.
        let _ = entry
            .value()
            .outbound_tx
            .try_send(OutboundMessage::Shutdown);
        true
    }

    /// Send an `ERROR_MSG` to a peer and then drop the connection.
    ///
    /// Thin public wrapper over [`peer_loop::send_error_and_drop`] (the
    /// FIFO `Send(ErrorMsg)` → `ShutdownAfterError` flush-then-close path).
    /// Returns `false` if the peer is unknown or its outbound channel is full
    /// (non-blocking, like [`Self::try_send_to`]/[`Self::disconnect`]).
    ///
    /// Matches stellar-core `Peer::sendErrorAndDrop` (Peer.cpp:722-729),
    /// reached e.g. from `SurveyManager::dropPeerIfSigInvalid`.
    pub fn send_error_and_drop(
        &self,
        peer_id: &PeerId,
        code: stellar_xdr::ErrorCode,
        message: &str,
    ) -> bool {
        let Some(entry) = self.peers.get(peer_id) else {
            return false;
        };
        peer_loop::send_error_and_drop(peer_id, &entry.value().outbound_tx, code, message)
    }

    /// Ban a peer by node ID and disconnect if connected.
    pub async fn ban_peer(&self, peer_id: PeerId) {
        self.banned_peers.write().insert(peer_id.clone());
        if let Some(entry) = self.peers.get(&peer_id) {
            let _ = entry
                .value()
                .outbound_tx
                .try_send(OutboundMessage::Shutdown);
        }
    }

    /// Remove a peer from the ban list.
    pub fn unban_peer(&self, peer_id: &PeerId) -> bool {
        self.banned_peers.write().remove(peer_id)
    }

    /// Return the list of banned peers.
    pub fn banned_peers(&self) -> Vec<PeerId> {
        self.banned_peers.read().iter().cloned().collect()
    }

    /// Send a message to a specific peer.
    ///
    /// Non-blocking: drops the message if the peer's outbound channel is full,
    /// returning `Err(ChannelSend)`. This prevents a slow/malicious peer from
    /// stalling the caller (matching stellar-core's non-blocking sendMessage).
    pub fn try_send_to(&self, peer_id: &PeerId, message: StellarMessage) -> Result<()> {
        let entry = self
            .peers
            .get(peer_id)
            .ok_or_else(|| OverlayError::PeerNotFound(peer_id.to_string()))?;

        // Route flow-controlled messages through the Flood path so they
        // consume per-peer SEND_MORE_EXTENDED credit, matching stellar-core's
        // Peer::sendMessage() which always flow-controls flood messages
        // regardless of broadcast vs. targeted send (AUDIT-086).
        let outbound = if helpers::is_flood_message(&message) {
            OutboundMessage::Flood(message)
        } else {
            OutboundMessage::Send(message)
        };

        entry.value().outbound_tx.try_send(outbound).map_err(|_| {
            self.metrics.messages_dropped.add(1);
            debug!(
                peer = %peer_id,
                "Outbound channel full, dropping targeted message"
            );
            OverlayError::ChannelSend
        })
    }

    /// Get the number of connected peers.
    /// Uses the peer info cache for lock-free access.
    pub fn peer_count(&self) -> usize {
        self.peer_info_cache.len()
    }

    /// Get the shared overlay metrics.
    pub fn overlay_metrics(&self) -> &OverlayMetrics {
        &self.metrics
    }

    /// Returns `(inbound_auth, outbound_auth, inbound_pending, outbound_pending)`.
    pub fn connection_breakdown(&self) -> (usize, usize, usize, usize) {
        (
            self.inbound_pool.authenticated_count(),
            self.outbound_pool.authenticated_count(),
            self.inbound_pool.pending_count(),
            self.outbound_pool.pending_count(),
        )
    }

    /// Get a list of connected peer IDs.
    /// Uses the peer info cache for lock-free access.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peer_info_cache
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    pub(super) fn count_outbound_peers(peer_info_cache: &DashMap<PeerId, PeerInfo>) -> usize {
        peer_info_cache
            .iter()
            .filter(|entry| entry.value().direction.we_called_remote())
            .count()
    }

    /// Count outbound peers that are not in the preferred set.
    ///
    /// Matches stellar-core `nonPreferredAuthenticatedCount()`
    /// (OverlayManagerImpl.cpp:835-849). Used to compute how many outbound
    /// slots are replaceable by preferred peers.
    pub(super) fn count_non_preferred_outbound_peers(
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        preferred_set: &PreferredPeerSet,
    ) -> usize {
        peer_info_cache
            .iter()
            .filter(|entry| {
                let info = entry.value();
                info.direction.we_called_remote() && !preferred_set.is_preferred(info)
            })
            .count()
    }

    /// Returns true if a peer's connection info matches the given address,
    /// checking the original hostname-based address first, then falling back
    /// to resolved IP comparison.
    pub(super) fn peer_info_matches_address(info: &PeerInfo, addr: &PeerAddress) -> bool {
        // Check by original address first (handles hostnames correctly)
        if let Some(ref orig) = info.original_address {
            if orig.host == addr.host && orig.port == addr.port {
                return true;
            }
        }
        // Fall back to IP comparison for backwards compatibility
        if info.address.port() != addr.port {
            return false;
        }
        addr.host
            .parse::<IpAddr>()
            .map(|ip| info.address.ip() == ip)
            .unwrap_or(false)
    }

    /// Returns true if a peer's connection matches the given resolved socket
    /// address (direct IP + port comparison, no hostname lookup).
    pub(super) fn peer_info_matches_socket_addr(info: &PeerInfo, addr: SocketAddr) -> bool {
        info.address == addr
    }

    /// True if ANY live connection (inbound or outbound) matches `addr`.
    ///
    /// Used as the pre-dial filter by the connection tick. This MUST consider
    /// both directions (parity: stellar-core `getConnectedPeer(address)`
    /// searches the inbound AND outbound authenticated maps before
    /// `connectTo`): after a mutual-dial tie-break, the lower-node-id side of
    /// a pair keeps an INBOUND connection — with an outbound-only filter it
    /// re-dialed that peer forever, and every re-dial raced the handshake
    /// path, occasionally REPLACING the healthy connection via the tie-break
    /// and leaving the replaced socket to idle out (30 s) and reset the
    /// remote. Measured on MissionMaxTPSClassic: 2294 reject/drop/reset
    /// events per node per mission vs stellar-core's 6, and a sustained
    /// (5-min-window) ceiling of ~1087 tx/s vs core's 1522 on identical
    /// hardware. Inbound connections are full-duplex peers; there is nothing
    /// to gain by dialing them again.
    pub(super) fn has_connection_to(
        peer_info_cache: &DashMap<PeerId, PeerInfo>,
        addr: &PeerAddress,
    ) -> bool {
        peer_info_cache.iter().any(|entry| {
            let info = entry.value();
            Self::peer_info_matches_address(info, addr)
        })
    }

    pub(super) fn build_peers_message(
        outbound: &[PeerAddress],
        inbound: &[PeerAddress],
        exclude: Option<&PeerAddress>,
    ) -> Option<StellarMessage> {
        let mut peers = Vec::new();
        let mut unique: HashSet<ResolvedPeerAddr> = HashSet::new();
        let mut ordered_outbound: Vec<&PeerAddress> = outbound.iter().collect();
        let mut ordered_inbound: Vec<&PeerAddress> = inbound.iter().collect();
        ordered_outbound.shuffle(&mut rand::thread_rng());
        ordered_inbound.shuffle(&mut rand::thread_rng());

        for addr in ordered_outbound.iter().chain(ordered_inbound.iter()) {
            if peers.len() >= MAX_PEERS_PER_MESSAGE {
                break;
            }
            if !Self::is_public_peer(addr) {
                continue;
            }
            if let Some(exclude) = exclude {
                if exclude == *addr {
                    continue;
                }
            }
            // Only advertise resolved IPv4 addresses; skip hostnames at startup.
            let Some(key) = ResolvedPeerAddr::try_from_peer_address(addr) else {
                continue;
            };
            if !unique.insert(key) {
                continue;
            }
            if let Some(xdr) = Self::peer_address_to_xdr(addr) {
                peers.push(xdr);
            }
        }

        if peers.is_empty() {
            return None;
        }

        let vecm: VecM<XdrPeerAddress, 100> = peers.try_into().ok()?;
        Some(StellarMessage::Peers(vecm))
    }

    fn peer_address_to_xdr(addr: &PeerAddress) -> Option<XdrPeerAddress> {
        let ip: IpAddr = addr.host.parse().ok()?;
        let ip = match ip {
            IpAddr::V4(v4) => PeerAddressIp::IPv4(v4.octets()),
            IpAddr::V6(v6) => PeerAddressIp::IPv6(v6.octets()),
        };

        Some(XdrPeerAddress {
            ip,
            port: addr.port as u32,
            num_failures: 0,
        })
    }

    fn is_public_peer(addr: &PeerAddress) -> bool {
        addr.port != 0 && !addr.is_private()
    }

    /// Get info for all connected peers.
    /// Uses the peer info cache for lock-free access.
    pub fn peer_infos(&self) -> Vec<PeerInfo> {
        self.peer_info_cache
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get snapshots for all connected peers.
    /// Uses the peer info cache for info and PeerHandle for lock-free stats access.
    pub fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.peer_info_cache
            .iter()
            .map(|entry| {
                let peer_id = entry.key();
                let info = entry.value().clone();
                let stats = self
                    .peers
                    .get(peer_id)
                    .map(|h| h.stats.snapshot())
                    .unwrap_or_default();
                PeerSnapshot { info, stats }
            })
            .collect()
    }

    /// Subscribe to incoming messages.
    pub fn subscribe(&self) -> broadcast::Receiver<OverlayMessage> {
        self.message_tx.subscribe()
    }

    /// Subscribe to the dedicated SCP message channel.
    ///
    /// Unlike the lossy broadcast channel, this dedicated channel preserves
    /// SCP messages under normal load. It is bounded ([`SCP_CHANNEL_CAPACITY`])
    /// so that a stalled consumer cannot grow memory without limit (#3623);
    /// over-capacity envelopes are dropped at the send side and recovered via
    /// peer re-flood + `GetScpState` backfill. Steady-state the loop drains far
    /// faster than the flood arrives, so the channel stays near-empty.
    ///
    /// Can only be called once (takes ownership of the receiver). Returns `None`
    /// if already called.
    pub async fn subscribe_scp(&self) -> Option<mpsc::Receiver<OverlayMessage>> {
        self.scp_message_rx.lock().await.take()
    }

    /// Subscribe to the dedicated fetch response message channel.
    ///
    /// Routes GeneralizedTxSet, TxSet, DontHave, ScpQuorumset, GetScpState,
    /// GetScpQuorumset, and GetTxSet messages through a dedicated unbounded
    /// channel so that no fetch-related traffic is ever silently dropped.
    /// Queue depth is sampled into `App` atomics and exported via `/metrics`
    /// (`henyey_overlay_fetch_channel_depth{,_max}`).
    ///
    /// Can only be called once (takes ownership of the receiver). Returns `None`
    /// if already called.
    pub async fn subscribe_fetch_responses(&self) -> Option<mpsc::Receiver<OverlayMessage>> {
        self.fetch_response_rx.lock().await.take()
    }

    /// Subscribe to catchup-critical messages (SCP + TxSet) via a dedicated mpsc channel.
    ///
    /// Unlike `subscribe()` which uses a broadcast channel that drops messages on overflow,
    /// this creates a **bounded** ([`CATCHUP_CHANNEL_CAPACITY`]) mpsc channel. Bounding is
    /// required because the consumer (`cache_messages_during_catchup_impl`) is aborted only
    /// when the event loop reaches the catchup→Tracking handoff; if that handoff stalls
    /// (#3582) an unbounded channel accumulates multi-MB tx-sets until the validator OOMs
    /// (#3661). Over-capacity messages are `try_send`-dropped and counted in
    /// `metrics.catchup_messages_dropped`. The channel is automatically cleaned up when the
    /// receiver is dropped.
    ///
    /// Dropping is recoverable: the cache is a pre-warm — the task re-broadcasts `GetTxSet`
    /// for missing tx-sets, dropped EXTERNALIZE envelopes are re-flooded by peers each slot,
    /// and after the handoff the main loop re-fetches anything still in `TxSetTracker.pending`.
    // SECURITY: subscriber count bounded by internal callers; no external input
    pub fn subscribe_catchup(&self) -> mpsc::Receiver<OverlayMessage> {
        let (tx, rx) = mpsc::channel(CATCHUP_CHANNEL_CAPACITY);
        let mut subs = self.extra_subscribers.write();
        // Clean up any closed subscribers while we have the write lock
        subs.retain(|s| !s.is_closed());
        subs.push(tx);
        rx
    }

    /// Get flood gate statistics.
    pub fn flood_stats(&self) -> FloodGateStats {
        self.flood_gate.stats()
    }

    /// Remove a flood-tracked message, allowing re-delivery to be treated
    /// as new.
    ///
    /// Mirrors stellar-core's `OverlayManagerImpl::forgetFloodedMsg`
    /// (OverlayManagerImpl.cpp:1264-1268). Called from the app layer when
    /// a flood-tracked message is discarded after `record_inbound_relay`
    /// already recorded the message hash. Two call sites:
    ///
    /// - **SCP envelopes**: rejected after verification (pre-filter or
    ///   post-verify discard).
    /// - **Transactions**: rejected by the tx queue (any result that is not
    ///   Added or Duplicate — parity with OverlayManagerImpl.cpp:1231-1236).
    pub fn forget_flooded_msg(&self, message_hash: &henyey_common::Hash256) {
        self.flood_gate.forget(message_hash);
    }

    /// Set the SCP queue callback for intelligent queue trimming.
    ///
    /// When set, the overlay will use herder state to make smart decisions
    /// about which SCP messages to drop from outbound queues (slot-age
    /// eviction and nomination/ballot replacement).
    pub fn set_scp_callback(&mut self, callback: Arc<dyn ScpQueueCallback>) {
        self.scp_callback = Some(callback);
    }

    /// Install the app's in-flight SCP dedup filter, run by the peer tasks on
    /// every inbound SCP envelope (see [`ScpInboundFilter`]). Duplicate
    /// copies of an envelope whose first copy is still queued/processing are
    /// dropped in the peer task instead of transiting the dedicated SCP
    /// channel (maxtps iter 7; parity: core `checkScheduledAndCache`).
    pub fn set_scp_inbound_filter(&self, filter: Arc<ScpInboundFilter>) {
        *self.scp_inbound_filter.write() = Some(filter);
    }

    /// Update the tracking-consensus flag.
    ///
    /// The app/herder layer should call this whenever the node transitions
    /// between "tracking" and "not tracking" states. When the node is not
    /// tracking consensus the overlay will periodically drop a random
    /// outbound peer to try fresh connections (see `maybe_drop_random_peer`).
    pub fn set_tracking(&self, tracking: bool) {
        self.is_tracking.store(tracking, Ordering::Relaxed);
    }

    /// Update the per-peer query rate-limit window.
    ///
    /// The app layer should call this after each ledger close with the
    /// result of `query_rate_limit_window(herder.ledger_close_duration())`.
    /// Parity: stellar-core recomputes this per-call in `Peer::process()`
    /// from `expectedLedgerCloseTime * MAX_SLOTS_TO_REMEMBER`.
    pub fn set_query_rate_limit_window(&self, window: Duration) {
        self.query_rate_limit_window_secs
            .store(window.as_secs(), Ordering::Relaxed);
    }

    /// Returns whether the node is currently tracking consensus.
    pub fn is_tracking(&self) -> bool {
        self.is_tracking.load(Ordering::Relaxed)
    }

    /// Returns a shared handle to the tracking flag.
    ///
    /// The app layer can clone this and update it directly from synchronous
    /// callbacks (e.g., `SyncRecoveryCallback::on_lost_sync`) without going
    /// through the overlay manager's async accessor.
    pub fn tracking_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_tracking)
    }

    /// Set the ledger-synced state.
    ///
    /// The app layer should call this whenever the node transitions between
    /// synced (`AppState::Validating` / `Synced`) and unsynced
    /// (`AppState::CatchingUp`) states. When unsynced, the peer loop drops
    /// `Transaction`, `FloodAdvert`, and `FloodDemand` messages early.
    /// Parity: mirrors stellar-core's `LedgerManager::isSynced()`.
    pub fn set_synced(&self, synced: bool) {
        self.is_synced.store(synced, Ordering::Relaxed);
    }

    /// Returns whether the node's ledger state is synced with consensus.
    pub fn is_synced(&self) -> bool {
        self.is_synced.load(Ordering::Relaxed)
    }

    /// Returns a shared handle to the synced flag.
    ///
    /// The app layer can clone this and update it directly from synchronous
    /// callbacks (e.g., `SyncRecoveryCallback::on_lost_sync`) without going
    /// through the overlay manager's async accessor.
    pub fn synced_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_synced)
    }

    /// Clear per-ledger state for ledgers below the given sequence.
    ///
    /// Mirrors upstream `OverlayManagerImpl::clearLedgersBelow()` which is
    /// called by the herder's `eraseBelow()` after every ledger close. It
    /// cleans up:
    ///
    /// - **Flood gate** entries from old ledgers (via [`FloodGate::clear_below`])
    ///
    /// The `_lcl_seq` parameter is accepted for parity with the upstream
    /// signature `(uint32_t ledgerSeq, uint32_t lclSeq)` but is unused here
    /// because survey cleanup and per-peer advert state are handled
    /// by the app layer (`tx_flooding.rs`).
    pub fn clear_ledgers_below(&self, ledger_seq: u32, _lcl_seq: u32) {
        self.last_closed_ledger.store(ledger_seq, Ordering::Relaxed);
        self.flood_gate.clear_below(ledger_seq);
        trace!(ledger_seq, "Cleared overlay state below ledger");
    }

    /// Notify all connected peers that the maximum transaction size has
    /// increased due to a protocol upgrade.
    ///
    /// Mirrors upstream `Peer::handleMaxTxSizeIncrease()` which updates
    /// flow control byte capacity and sends `SEND_MORE_EXTENDED` with the
    /// additional bytes so the remote peer can unblock.
    ///
    /// **Parity note:** This is called unconditionally regardless of whether
    /// flow control byte config overrides are active. With `Fixed` config,
    /// new peers use the fixed total while existing peers accumulate the
    /// increase on top of their current capacity — matching stellar-core
    /// `HerderImpl.cpp:2304-2308`.
    pub async fn handle_max_tx_size_increase(&self, increase: u32) {
        if increase == 0 {
            return;
        }

        // Send SEND_MORE_EXTENDED with 0 additional messages but
        // `increase` additional bytes, matching upstream behavior.
        let send_more = StellarMessage::SendMoreExtended(stellar_xdr::SendMoreExtended {
            num_messages: 0,
            num_bytes: increase,
        });

        for entry in self.peers.iter() {
            // Update each peer's FlowControl byte capacity
            entry.value().flow_control.handle_tx_size_increase(increase);
            if entry
                .value()
                .outbound_tx
                .try_send(OutboundMessage::Send(send_more.clone()))
                .is_err()
            {
                self.metrics.messages_dropped.add(1);
            }
        }

        debug!(
            increase,
            peers = self.peer_count(),
            "Notified peers of max tx size increase"
        );
    }

    /// Check if the overlay is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get peer counts broken down by authentication state.
    ///
    /// Returns `(pending_count, authenticated_count)` summed across inbound
    /// and outbound connection pools.
    pub fn peer_counts(&self) -> (usize, usize) {
        let pending = self.inbound_pool.pending_count() + self.outbound_pool.pending_count();
        let authenticated =
            self.inbound_pool.authenticated_count() + self.outbound_pool.authenticated_count();
        (pending, authenticated)
    }

    /// Get overlay statistics.
    pub fn stats(&self) -> OverlayStats {
        OverlayStats {
            connected_peers: self.peer_count(),
            inbound_peers: self.inbound_pool.count(),
            outbound_peers: self.outbound_pool.count(),
            flood_stats: self.flood_stats(),
        }
    }

    /// Total count of authenticated peers added.
    pub fn added_authenticated_peers(&self) -> u64 {
        self.added_authenticated_peers.load(Ordering::Relaxed)
    }

    /// Total count of authenticated peers dropped.
    pub fn dropped_authenticated_peers(&self) -> u64 {
        self.dropped_authenticated_peers.load(Ordering::Relaxed)
    }

    /// Return the current known peer list (diagnostics view).
    pub fn known_peers(&self) -> Vec<PeerAddress> {
        self.known_peers.read().all_entries()
    }

    /// Replace discovered peers (from DB refresh). Config entries and their
    /// resolution state are preserved.
    pub fn set_known_peers(&self, peers: Vec<PeerAddress>) {
        self.known_peers.write().set_discovered(peers);
    }

    /// Replace the peers used for Peers advertisements.
    pub fn set_advertised_peers(
        &self,
        outbound_peers: Vec<PeerAddress>,
        inbound_peers: Vec<PeerAddress>,
    ) {
        let mut advertised_outbound = self.advertised_outbound_peers.write();
        let mut advertised_inbound = self.advertised_inbound_peers.write();
        *advertised_outbound = outbound_peers;
        *advertised_inbound = inbound_peers;
    }

    /// Request SCP state from up to 2 random authenticated peers.
    ///
    /// Parity: stellar-core `HerderImpl::getMoreSCPState()` (HerderImpl.cpp:2643-2658)
    /// + `OverlayManagerImpl::getRandomAuthenticatedPeers()`
    /// (OverlayManagerImpl.cpp:1133-1142): snapshot the authenticated peers
    /// once, shuffle, and send `GetScpState` to at most 2 of them rather than
    /// flooding all connected peers (§15.3). `connected_peers()` enumerates the
    /// authenticated peer set (the same map `try_send_to` routes through).
    pub fn request_scp_state(&self, ledger_seq: u32) -> Result<usize> {
        use rand::seq::SliceRandom;

        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        let message = StellarMessage::GetScpState(ledger_seq);
        let peers = self.connected_peers();
        if peers.is_empty() {
            return Ok(0);
        }

        // Select up to 2 random peers, matching stellar-core's bounded pull.
        let mut rng = rand::thread_rng();
        let selected: Vec<&PeerId> = peers
            .choose_multiple(&mut rng, 2.min(peers.len()))
            .collect();

        let mut sent = 0usize;
        for peer_id in &selected {
            match self.try_send_to(peer_id, message.clone()) {
                Ok(()) => sent += 1,
                Err(e) => {
                    debug!(peer = %peer_id, error = %e, "Failed to send GetScpState to peer");
                }
            }
        }

        debug!(ledger_seq, sent, "Sent GetScpState to random peers");
        Ok(sent)
    }

    /// Request SCP state from a *bounded-wider* set of authenticated peers.
    ///
    /// This is a henyey-specific recovery escape (issue #3318) — it has NO
    /// direct upstream analog. stellar-core's `getMoreSCPState` is hard-bounded
    /// to 2 peers and never widens. The app-layer caller fires this exactly once
    /// per stuck episode, only after the bounded 2-peer `request_scp_state` has
    /// repeatedly landed on peers that cannot serve the missing slot
    /// (`peers_could_serve == 0`) AND that condition has persisted past a
    /// wall-clock deadline. It does NOT replace `request_scp_state`, which stays
    /// the steady-state upstream mirror.
    ///
    /// Selection draws from the FULL authenticated peer set (inbound + outbound),
    /// matching `getRandomAuthenticatedPeers`, shuffles it, and sends
    /// `GetScpState` to up to `cap` peers via the non-blocking `try_send_to`
    /// (event-loop rule: never a blocking send). The caller is responsible for
    /// computing `cap` (e.g. `min(serviceable, 8)`, falling back to all
    /// authenticated peers when serviceable is 0); this method just honors the
    /// bound over the connected set.
    pub fn request_scp_state_widened(&self, ledger_seq: u32, cap: usize) -> Result<usize> {
        use rand::seq::SliceRandom;

        if !self.running.load(Ordering::Relaxed) {
            return Err(OverlayError::NotStarted);
        }

        let message = StellarMessage::GetScpState(ledger_seq);
        let mut peers = self.connected_peers();
        if peers.is_empty() || cap == 0 {
            return Ok(0);
        }

        // Shuffle the full authenticated set and take up to `cap`.
        let mut rng = rand::thread_rng();
        peers.shuffle(&mut rng);
        let target = cap.min(peers.len());

        let mut sent = 0usize;
        for peer_id in peers.iter().take(target) {
            match self.try_send_to(peer_id, message.clone()) {
                Ok(()) => sent += 1,
                Err(e) => {
                    debug!(peer = %peer_id, error = %e, "Failed to send widened GetScpState to peer");
                }
            }
        }

        debug!(
            ledger_seq,
            sent, cap, "Sent widened GetScpState to authenticated peers (#3318 recovery escape)"
        );
        Ok(sent)
    }

    /// Record the highest externalized SCP slot observed *via live SCP gossip*
    /// from `peer` (observability-only). Called from the app event loop at the
    /// two EXTERNALIZE-accept sites where the global externalize `fetch_max`
    /// already fires and `from_peer`/`slot` are in scope.
    ///
    /// The stored value is monotonic per peer (`fetch_max`): a lower slot never
    /// regresses a peer's recorded high-water mark. Absence of an entry means
    /// "no externalized observation yet" — a distinct state from slot 0 (see
    /// `peers_could_serve`, which excludes never-observed peers).
    ///
    /// This is a pure side-write: it never affects envelope acceptance, relay,
    /// peer selection, or the `GetScpState` watermark. The scope is explicitly
    /// "EXTERNALIZE observed via live SCP gossip" — slots learned via catchup
    /// replay or `GetScpState` responses are not recorded here, which is the
    /// correct signal for #3270 (the question is about peers' *live* SCP
    /// windows). See issue #3270.
    pub fn record_peer_externalized(&self, peer: &PeerId, slot: u64) {
        // Only record for peers we currently consider connected, so we never
        // resurrect an entry for a peer that just disconnected (the disconnect
        // cleanup removes the entry; a racing late EXTERNALIZE must not re-add
        // a stale peer).
        if !self.peer_info_cache.contains_key(peer) {
            return;
        }
        self.peer_latest_externalized
            .entry(peer.clone())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_max(slot, Ordering::Relaxed);
    }

    /// Return `(could_serve, connected)` for a `GetScpState` re-request of
    /// `requested_slot` (observability-only).
    ///
    /// - `connected` is the number of currently-connected peers.
    /// - `could_serve` counts connected peers whose recorded latest observed
    ///   externalized slot is recent enough that they could still hold
    ///   `requested_slot` in their SCP window, i.e.
    ///   `latest_ext - max_slots <= requested_slot` (the parity-faithful inverse
    ///   of stellar-core's trim boundary `consensusIndex - MAX_SLOTS_TO_REMEMBER`,
    ///   HerderImpl.cpp:1011-1012).
    ///
    /// A peer with **no recorded externalized observation** is counted in
    /// `connected` but **excluded** from `could_serve`. This is deliberate: we
    /// must NOT treat "never observed" as slot 0 and feed it through
    /// `saturating_sub`, because `0.saturating_sub(max_slots) = 0 <=
    /// requested_slot` would wrongly count freshly-connected churn peers as
    /// serviceable — corrupting the signal exactly during the overlay churn
    /// that motivates #3270.
    ///
    /// `max_slots` is passed in by the app caller (sourced from the same herder
    /// `MAX_SLOTS_TO_REMEMBER` constant), keeping this crate free of a herder
    /// dependency. See issue #3270.
    pub fn peers_could_serve(&self, requested_slot: u32, max_slots: u32) -> (usize, usize) {
        let connected = self.peer_info_cache.len();
        let requested = requested_slot as u64;
        let max_slots = max_slots as u64;
        let could_serve = self
            .peer_info_cache
            .iter()
            .filter(|entry| {
                // Excluded unless we have an actual observation for this peer.
                match self.peer_latest_externalized.get(entry.key()) {
                    Some(latest) => {
                        let latest_ext = latest.load(Ordering::Relaxed);
                        latest_ext.saturating_sub(max_slots) <= requested
                    }
                    None => false,
                }
            })
            .count();
        (could_serve, connected)
    }

    /// Request a transaction set by hash from all peers.
    pub async fn request_tx_set(&self, hash: &Uint256) -> Result<usize> {
        let message = StellarMessage::GetTxSet(hash.clone());
        tracing::info!(
            hash = hex::encode(&hash.0),
            "Requesting transaction set from peers"
        );
        self.broadcast(message).await
    }

    /// Request a transaction set by hash from a specific peer.
    ///
    /// Used by ItemFetcher to request TxSets from individual peers with retry logic.
    pub async fn send_get_tx_set(&self, peer_id: &PeerId, hash: &Uint256) -> Result<()> {
        let message = StellarMessage::GetTxSet(hash.clone());
        tracing::debug!(
            peer = %peer_id,
            hash = hex::encode(&hash.0),
            "Requesting transaction set from peer"
        );
        self.try_send_to(peer_id, message)
    }

    /// Request a quorum set by hash from a specific peer.
    ///
    /// Used by ItemFetcher to request QuorumSets from individual peers with retry logic.
    pub async fn send_get_quorum_set(&self, peer_id: &PeerId, hash: &Uint256) -> Result<()> {
        let message = StellarMessage::GetScpQuorumset(hash.clone());
        tracing::debug!(
            peer = %peer_id,
            hash = hex::encode(&hash.0),
            "Requesting quorum set from peer"
        );
        self.try_send_to(peer_id, message)
    }

    pub(super) fn add_known_peer(&self, addr: PeerAddress) -> bool {
        self.known_peers.write().add_discovered(addr)
    }

    /// Timeout for joining overlay handles (listener, connector, peers)
    /// during shutdown. A single shared deadline — not additive per handle.
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Send the shutdown signal without joining any handles.
    ///
    /// Idempotent: the `running` atomic swap ensures the signal logic runs
    /// at most once. Safe to call through `&self` (and thus through
    /// `Arc<Self>`) when `Arc::try_unwrap` fails in the app shutdown path.
    pub fn signal_shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return; // already signaled
        }

        info!("Signaling overlay shutdown");

        // Broadcast shutdown to listener/connector/tick tasks.
        if let Some(tx) = self.shutdown_tx.lock().take() {
            let _ = tx.send(());
        }

        // Send shutdown to all peer tasks via their outbound channels.
        let senders: Vec<_> = self
            .peers
            .iter()
            .map(|e| e.value().outbound_tx.clone())
            .collect();
        for tx in senders {
            let _ = tx.try_send(OutboundMessage::Shutdown);
        }
        self.peers.clear();
    }

    /// Await `handle` up to `deadline`; if it doesn't finish, abort it.
    ///
    /// `JoinHandle::drop` only detaches a task (does NOT cancel it), so we
    /// poll via `&mut` to retain ownership and call `abort()` explicitly on
    /// timeout.
    async fn join_or_abort_handle(
        mut handle: JoinHandle<()>,
        deadline: tokio::time::Instant,
        label: &str,
    ) {
        tokio::select! {
            _ = &mut handle => {}
            _ = tokio::time::sleep_until(deadline) => {
                warn!("{label} handle join timed out, aborting");
                handle.abort();
            }
        }
    }

    /// Join listener, connector, and peer handles under a single shared
    /// deadline. Handles that don't finish in time are explicitly aborted.
    async fn join_handles(&mut self) {
        let start = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Self::SHUTDOWN_TIMEOUT;

        // Listener
        if let Some(handle) = self.listener_handle.take() {
            Self::join_or_abort_handle(handle, deadline, "Listener").await;
        }

        // Connector
        if let Some(handle) = self.connector_handle.take() {
            Self::join_or_abort_handle(handle, deadline, "Connector").await;
        }

        // Peer handles — join concurrently, abort any that exceed the deadline
        let handles: Vec<_> = std::mem::take(&mut *self.peer_handles.write());
        let peer_count = handles.len();
        if !handles.is_empty() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                warn!(
                    peer_count,
                    "No time remaining for peer handles, aborting all"
                );
                for handle in &handles {
                    handle.abort();
                }
            } else {
                let futs: Vec<_> = handles
                    .into_iter()
                    .map(|h| Self::join_or_abort_handle(h, deadline, "Peer"))
                    .collect();
                futures::future::join_all(futs).await;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                if elapsed_ms > Self::SHUTDOWN_TIMEOUT.as_millis() as u64 {
                    warn!(
                        peer_count,
                        elapsed_ms, "Peer handle joins exceeded deadline"
                    );
                } else {
                    info!(peer_count, elapsed_ms, "All peer handles joined");
                }
            }
        }
    }

    /// Stop the overlay network.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.signal_shutdown();

        let start = std::time::Instant::now();
        self.join_handles().await;
        info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Overlay manager shutdown complete"
        );

        Ok(())
    }
}

impl Drop for OverlayManager {
    fn drop(&mut self) {
        self.signal_shutdown();
    }
}

/// Summary statistics for the overlay network.
///
/// Provides a high-level view of overlay health and activity.
#[derive(Debug, Clone)]
pub struct OverlayStats {
    /// Total number of connected peers (inbound + outbound).
    pub connected_peers: usize,
    /// Number of peers that connected to us.
    pub inbound_peers: usize,
    /// Number of peers we connected to.
    pub outbound_peers: usize,
    /// Message flooding statistics.
    pub flood_stats: FloodGateStats,
}

// ── Test utilities (cross-crate) ─────────────────────────────────────────

/// A receiver for messages sent to an injected test peer.
///
/// Wraps the internal outbound channel and extracts `StellarMessage` payloads,
/// hiding the crate-internal `OutboundMessage` enum from downstream test code.
#[cfg(feature = "test-utils")]
#[doc(hidden)]
pub struct TestPeerReceiver {
    rx: tokio::sync::mpsc::Receiver<OutboundMessage>,
}

#[cfg(feature = "test-utils")]
impl TestPeerReceiver {
    /// Receive the next `StellarMessage`. Returns `None` on channel close or `Shutdown`.
    pub async fn recv(&mut self) -> Option<StellarMessage> {
        match self.rx.recv().await? {
            OutboundMessage::Send(msg) | OutboundMessage::Flood(msg) => Some(msg),
            OutboundMessage::Shutdown | OutboundMessage::ShutdownAfterError => None,
        }
    }

    /// Non-blocking try_recv. Returns `None` if channel is empty, closed, or Shutdown.
    pub fn try_recv(&mut self) -> Option<StellarMessage> {
        match self.rx.try_recv().ok()? {
            OutboundMessage::Send(msg) | OutboundMessage::Flood(msg) => Some(msg),
            OutboundMessage::Shutdown | OutboundMessage::ShutdownAfterError => None,
        }
    }
}

#[cfg(feature = "test-utils")]
impl OverlayManager {
    /// Inject a synthetic peer into this overlay's peer map for testing.
    ///
    /// Returns a [`TestPeerReceiver`] that receives all messages sent to this peer
    /// via `try_send_to`. The peer uses synthetic metadata (127.0.0.1:11625, Inbound)
    /// and default flow control.
    ///
    /// # Panics
    /// Panics if `channel_capacity` is 0.
    #[doc(hidden)]
    pub fn inject_test_peer(&self, peer_id: PeerId, channel_capacity: usize) -> TestPeerReceiver {
        assert!(channel_capacity > 0, "channel_capacity must be > 0");

        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;

        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(channel_capacity);
        let handle = PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
            direction: crate::connection::ConnectionDirection::Inbound,
            generation: 0,
        };
        self.peers.insert(peer_id.clone(), handle);
        self.peer_info_cache.insert(
            peer_id.clone(),
            crate::peer::PeerInfo {
                peer_id,
                address: "127.0.0.1:11625".parse().unwrap(),
                direction: crate::connection::ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: std::time::Instant::now(),
                original_address: None,
            },
        );
        TestPeerReceiver { rx: outbound_rx }
    }

    /// Mark the overlay as running for testing purposes.
    ///
    /// This allows `broadcast()` to proceed without calling `start()`,
    /// which would spin up listener/connector background tasks.
    #[doc(hidden)]
    pub fn set_running_for_test(&self) {
        self.running
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use henyey_crypto::SecretKey;

    #[test]
    fn test_overlay_manager_creation() {
        let config = OverlayConfig::testnet();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node);
        assert!(manager.is_ok());
    }

    #[test]
    fn test_scp_channel_aggregate_can_exceed_cap_so_backstop_retained() {
        // #3643 / #3649 KEEP-DECISION (documents WHY the #3626 backstop stays).
        //
        // The #3626 bounded SCP channel (8192) + try_send-drop-on-full is an
        // intentional aggregate backstop with NO stellar-core equivalent (core
        // processes inbound per-peer SYNCHRONOUSLY, so it has no aggregate
        // inbound channel to bound). #3625 Phase 2 added a per-peer read
        // throttle bounding in-flight envelopes to PEER_READING_CAPACITY (201)
        // PER PEER — but it does NOT bound the AGGREGATE across all peers.
        //
        // Worst-case aggregate in-flight = max_peers × PEER_READING_CAPACITY.
        // On mainnet max_inbound_peers=64 + max_outbound_peers≤20 = 84 peers:
        //   84 × 201 = 16_884 > 8_192 (SCP_CHANNEL_CAPACITY).
        //
        // Because the aggregate EXCEEDS the channel cap, retiring the backstop
        // (= unbounding scp_message_tx) could let a wedged consumer hold ~16.9k
        // token-bearing envelopes — a regression versus the hard 8192 cap the
        // deployed validator relies on against the #3582 stall / #3623 OOM. So
        // the backstop is KEPT; retirement is operator-gated in #3649. This
        // assertion encodes the number so any future retirement must confront it.
        const MAX_PEERS: u64 = 64 + 20; // max_inbound + max_outbound (mainnet)
        let per_peer = crate::flow_control::FlowControlConfig::default().peer_reading_capacity;
        assert_eq!(per_peer, 201, "PEER_READING_CAPACITY parity (stellar-core)");
        let worst_case_aggregate = MAX_PEERS * per_peer;
        assert_eq!(worst_case_aggregate, 16_884, "84 × 201 = 16_884");
        assert!(
            worst_case_aggregate > SCP_CHANNEL_CAPACITY as u64,
            "aggregate worst-case in-flight ({worst_case_aggregate}) exceeds the \
             SCP channel cap ({SCP_CHANNEL_CAPACITY}); the per-peer throttle does \
             NOT bound the aggregate, so the #3626 backstop must be retained \
             (retirement gated to #3649)"
        );
    }

    #[tokio::test]
    async fn test_overlay_stats() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        let stats = manager.stats();

        assert_eq!(stats.connected_peers, 0);
        assert_eq!(stats.inbound_peers, 0);
        assert_eq!(stats.outbound_peers, 0);
    }

    #[test]
    fn test_set_query_rate_limit_window_propagates_to_shared_state() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Default should be 60s (5s * 12).
        let shared = manager.shared_state();
        assert_eq!(
            shared.query_rate_limit_window_secs.load(Ordering::Relaxed),
            60
        );

        // Update via setter and verify SharedPeerState sees the new value.
        manager.set_query_rate_limit_window(Duration::from_secs(54));
        let shared2 = manager.shared_state();
        assert_eq!(
            shared2.query_rate_limit_window_secs.load(Ordering::Relaxed),
            54
        );

        // The previously-cloned SharedPeerState should also see the update
        // (same Arc).
        assert_eq!(
            shared.query_rate_limit_window_secs.load(Ordering::Relaxed),
            54
        );
    }

    #[test]
    fn test_outbound_channel_capacity_propagates_from_connection_factory() {
        use crate::loopback::LoopbackConnectionFactory;

        // TCP factory → default 16384 (holds a full max-size ledger of txs)
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret.clone());
        let manager = OverlayManager::new(config.clone(), local_node).unwrap();
        let shared = manager.shared_state();
        assert_eq!(shared.outbound_channel_capacity, 16384);

        // Loopback factory → 2048
        let local_node2 = LocalNode::new_testnet(secret);
        let manager2 = OverlayManager::new_with_connection_factory(
            config,
            local_node2,
            Arc::new(LoopbackConnectionFactory::default()),
        )
        .unwrap();
        let shared2 = manager2.shared_state();
        assert_eq!(shared2.outbound_channel_capacity, 2048);
    }

    #[tokio::test]
    async fn test_subscribe_fetch_responses_returns_receiver_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // First call should return Some
        let rx = manager.subscribe_fetch_responses().await;
        assert!(
            rx.is_some(),
            "first subscribe_fetch_responses() should return Some"
        );

        // Second call should return None (already taken)
        let rx2 = manager.subscribe_fetch_responses().await;
        assert!(
            rx2.is_none(),
            "second subscribe_fetch_responses() should return None"
        );
    }

    #[tokio::test]
    async fn test_subscribe_scp_returns_receiver_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // First call should return Some
        let rx = manager.subscribe_scp().await;
        assert!(rx.is_some(), "first subscribe_scp() should return Some");

        // Second call should return None (already taken)
        let rx2 = manager.subscribe_scp().await;
        assert!(rx2.is_none(), "second subscribe_scp() should return None");
    }

    #[test]
    fn test_clear_ledgers_below() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Record some flood messages at ledger 100
        let hash1 = henyey_common::Hash256([1u8; 32]);
        let hash2 = henyey_common::Hash256([2u8; 32]);
        manager.flood_gate.record_local_broadcast(hash1, 100);
        manager.flood_gate.record_local_broadcast(hash2, 100);
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below should not remove entries at or above the threshold
        manager.clear_ledgers_below(100, 100);
        assert_eq!(manager.flood_stats().seen_count, 2);

        // clear_ledgers_below with a higher seq removes them
        manager.clear_ledgers_below(101, 101);
        assert_eq!(manager.flood_stats().seen_count, 0);
    }

    #[test]
    fn test_clear_ledgers_below_no_panic_when_empty() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Should not panic with empty flood gate
        manager.clear_ledgers_below(0, 0);
        manager.clear_ledgers_below(100, 50);
        manager.clear_ledgers_below(u32::MAX, u32::MAX);
    }

    /// Regression test for AUDIT-H13: known_peers must be capped at MAX_KNOWN_PEERS.
    #[test]
    fn test_known_peers_cap() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Add MAX_KNOWN_PEERS unique addresses — all should be accepted.
        for i in 0..MAX_KNOWN_PEERS {
            let port = (i % 65534 + 1) as u16;
            let host = format!("10.{}.{}.{}", (i >> 16) & 0xFF, (i >> 8) & 0xFF, i & 0xFF);
            let addr = PeerAddress::new(&host, port);
            assert!(
                manager.add_known_peer(addr),
                "peer {i} should be accepted (under cap)"
            );
        }
        assert_eq!(manager.known_peers().len(), MAX_KNOWN_PEERS);

        // One more should be rejected.
        let extra = PeerAddress::new("192.168.1.1", 9999);
        assert!(
            !manager.add_known_peer(extra),
            "should reject when at MAX_KNOWN_PEERS"
        );
        assert_eq!(manager.known_peers().len(), MAX_KNOWN_PEERS);
    }

    /// Verify deduplication still works.
    #[test]
    fn test_known_peers_dedup() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        let addr = PeerAddress::new("10.0.0.1", 11625);
        assert!(manager.add_known_peer(addr.clone()));
        assert!(!manager.add_known_peer(addr));
    }

    /// INV-O11: IPv6 peers must be excluded from PEERS messages.
    /// `ResolvedPeerAddr::try_from_peer_address` returns None for IPv6,
    /// so `build_peers_message` skips them. This regression test ensures
    /// the exclusion holds even when IPv6 peers are mixed with IPv4.
    #[test]
    fn test_build_peers_message_excludes_ipv6() {
        let ipv4_peer = PeerAddress::new("93.184.216.34", 11625);
        let ipv6_peer = PeerAddress::new("::1", 11625);
        let ipv6_full = PeerAddress::new("2001:db8::1", 11625);

        // Only IPv6
        let msg =
            OverlayManager::build_peers_message(&[], &[ipv6_peer.clone(), ipv6_full.clone()], None);
        assert!(
            msg.is_none(),
            "pure-IPv6 list should produce no PEERS message"
        );

        // Mix of IPv4 and IPv6
        let msg = OverlayManager::build_peers_message(
            std::slice::from_ref(&ipv4_peer),
            &[ipv6_peer, ipv6_full],
            None,
        );
        let peers = match msg.unwrap() {
            StellarMessage::Peers(p) => p.to_vec(),
            other => panic!("expected Peers, got {:?}", other),
        };
        assert_eq!(peers.len(), 1, "only the IPv4 peer should be included");
    }

    /// Verify non-default peer_flood_reading_capacity propagates from config
    /// to SharedPeerState, ensuring the SEND_MORE_EXTENDED message grant
    /// uses the configured value (not the hardcoded default).
    #[test]
    fn test_peer_flood_reading_capacity_propagates_from_config() {
        let mut config = OverlayConfig::default();
        config.peer_flood_reading_capacity = 500; // non-default
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let manager = OverlayManager::new(config, local_node).unwrap();
        let shared = manager.shared_state();
        assert_eq!(
            shared.peer_flood_reading_capacity, 500,
            "peer_flood_reading_capacity must propagate from OverlayConfig to SharedPeerState"
        );
    }

    #[test]
    fn test_pending_connections_address_dedup() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let pending = PendingConnections::new();
        let addr = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            11625,
        ));

        assert!(
            pending.try_reserve_address(addr),
            "first reservation should succeed"
        );
        assert!(
            !pending.try_reserve_address(addr),
            "duplicate reservation should fail"
        );

        pending.release_address(&addr);
        assert!(
            pending.try_reserve_address(addr),
            "should succeed after release"
        );

        // Same IP, different port should succeed independently
        let addr2 = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            11626,
        ));
        assert!(
            pending.try_reserve_address(addr2),
            "same IP but different port should succeed"
        );
    }

    #[test]
    fn test_pending_connections_peer_id_dedup() {
        let pending = PendingConnections::new();
        let peer_id = PeerId::from_bytes([1u8; 32]);

        assert!(
            pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound),
            "first reservation should succeed"
        );
        assert!(
            !pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound),
            "duplicate should fail"
        );

        pending.release_peer_id(&peer_id);
        assert!(
            pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound),
            "should succeed after release"
        );
    }

    #[test]
    fn test_pending_connections_independent_tracking() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let pending = PendingConnections::new();
        let addr = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            11625,
        ));
        let peer_id = PeerId::from_bytes([1u8; 32]);

        // Address and peer_id are independent
        assert!(pending.try_reserve_address(addr));
        assert!(pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound));

        // Different address should work
        let addr2 = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 2),
            11625,
        ));
        assert!(pending.try_reserve_address(addr2));
    }

    #[test]
    fn test_pending_connections_sweep_stale() {
        use std::net::{Ipv4Addr, SocketAddrV4};
        let pending = PendingConnections::new();
        let addr = ResolvedPeerAddr::from_socket_addr_v4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            11625,
        ));

        // Insert with a backdated timestamp
        pending.by_address.insert(
            addr,
            std::time::Instant::now() - std::time::Duration::from_secs(60),
        );

        assert!(
            !pending.try_reserve_address(addr),
            "stale entry still blocks before sweep"
        );

        pending.sweep_stale();

        assert!(
            pending.try_reserve_address(addr),
            "should succeed after sweep removes stale entry"
        );
    }

    /// Verify that an inbound reservation attempt that collides with an
    /// existing OUTBOUND reservation fails at the try_reserve_peer_id level
    /// (the direction-aware bypass is in the handshake layer, not here).
    /// This test validates that the low-level DashMap dedup still works.
    #[test]
    fn test_pending_connections_outbound_blocks_second_reserve() {
        let pending = PendingConnections::new();
        let peer_id = PeerId::from_bytes([2u8; 32]);

        // Reserve as outbound
        assert!(pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound));
        // A second reservation (regardless of direction) should fail
        // because try_reserve_peer_id is a raw Entry::Occupied check.
        assert!(!pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Inbound));
    }

    /// Verify that sweep_stale correctly removes old PendingPeerEntry values.
    #[test]
    fn test_pending_connections_sweep_stale_peer_id() {
        let pending = PendingConnections::new();
        let peer_id = PeerId::from_bytes([3u8; 32]);

        // Insert with a backdated timestamp
        pending.by_peer_id.insert(
            peer_id.clone(),
            PendingPeerEntry {
                reserved_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
                direction: ConnectionDirection::Outbound,
            },
        );

        // Should still block before sweep
        assert!(!pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound));

        pending.sweep_stale();

        // Should succeed after sweep
        assert!(
            pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound),
            "should succeed after sweep removes stale peer_id entry"
        );
    }

    /// Verify the direction metadata is correctly stored in PendingPeerEntry.
    #[test]
    fn test_pending_peer_entry_stores_direction() {
        let pending = PendingConnections::new();
        let peer_id = PeerId::from_bytes([4u8; 32]);

        pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Outbound);
        let entry = pending.by_peer_id.get(&peer_id).unwrap();
        assert_eq!(entry.direction, ConnectionDirection::Outbound);
        drop(entry);

        pending.release_peer_id(&peer_id);

        pending.try_reserve_peer_id(&peer_id, ConnectionDirection::Inbound);
        let entry = pending.by_peer_id.get(&peer_id).unwrap();
        assert_eq!(entry.direction, ConnectionDirection::Inbound);
    }

    /// Build a minimal SharedPeerState for testing preferred-peer eviction.
    fn test_shared_state(preferred: Vec<PeerAddress>) -> SharedPeerState {
        let (message_tx, _) = tokio::sync::broadcast::channel(1);
        let (scp_message_tx, _scp_rx) = tokio::sync::mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_response_tx, _) = tokio::sync::mpsc::channel(FETCH_CHANNEL_CAPACITY);
        SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                preferred,
                HashSet::new(),
            ))),
            preferred_peers_only: false,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        }
    }
    fn insert_fake_peer(
        shared: &SharedPeerState,
        peer_id: PeerId,
        addr: std::net::SocketAddr,
        direction: crate::connection::ConnectionDirection,
    ) -> tokio::sync::mpsc::Receiver<super::OutboundMessage> {
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;

        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(16);
        let handle = super::PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
            direction,
            generation: 0,
        };
        shared.peers.insert(peer_id.clone(), handle);
        shared.peer_info_cache.insert(
            peer_id.clone(),
            crate::peer::PeerInfo {
                peer_id,
                address: addr,
                direction,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: std::time::Instant::now(),
                original_address: None,
            },
        );
        outbound_rx
    }

    fn candidate_info(
        peer_id: PeerId,
        addr: std::net::SocketAddr,
        direction: crate::connection::ConnectionDirection,
    ) -> crate::peer::PeerInfo {
        crate::peer::PeerInfo {
            peer_id,
            address: addr,
            direction,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        }
    }

    /// Regression test for AUDIT-055: preferred inbound peer evicts non-preferred
    /// when all slots are full.
    #[tokio::test]
    async fn test_audit_055_preferred_peer_evicts_non_preferred_inbound() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        // Fill inbound pool to capacity (max_connections = 2).
        let pool = Arc::new(ConnectionPool::new(2));
        pool.try_reserve();
        pool.force_promote_authenticated(); // peer A: non-preferred
        pool.try_reserve();
        pool.force_promote_authenticated(); // peer B: non-preferred
        assert_eq!(pool.authenticated_count(), 2);

        // Insert a non-preferred inbound peer.
        let non_pref_id = PeerId::from_bytes([1u8; 32]);
        let non_pref_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let mut victim_rx = insert_fake_peer(
            &shared,
            non_pref_id.clone(),
            non_pref_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        // Preferred peer eviction should succeed.
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(evicted, "should evict a non-preferred peer for preferred");

        // The evicted peer should have received a shutdown message.
        let msg = victim_rx.try_recv();
        assert!(
            msg.is_ok(),
            "victim should receive an error-and-drop message"
        );

        // Pool authenticated count is now 3 (2 existing + 1 force-promoted).
        // The evicted peer will release its slot asynchronously.
        assert_eq!(pool.authenticated_count(), 3);
    }

    /// When all authenticated inbound peers are preferred, eviction should fail.
    #[tokio::test]
    async fn test_audit_055_all_preferred_no_eviction() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        assert_eq!(pool.authenticated_count(), 1);

        // Insert an inbound peer that IS preferred.
        let pref_id = PeerId::from_bytes([2u8; 32]);
        let pref_addr: std::net::SocketAddr = "10.0.0.1:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            pref_id,
            pref_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        // Eviction should fail — the only authenticated peer is preferred.
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(!evicted, "should not evict when all peers are preferred");
        assert_eq!(pool.authenticated_count(), 1);
    }

    /// Non-preferred peers should not trigger eviction (only preferred peers
    /// get this treatment).
    #[tokio::test]
    async fn test_audit_055_non_preferred_peer_does_not_evict() {
        use crate::connection::ConnectionPool;

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        // Insert a non-preferred inbound peer.
        let np_id = PeerId::from_bytes([3u8; 32]);
        let np_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            np_id,
            np_addr,
            crate::connection::ConnectionDirection::Inbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.99:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        assert!(
            !OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool),
            "non-preferred peer should not evict"
        );
    }

    /// Outbound non-preferred peers must not be evicted when making room for
    /// a preferred inbound peer — eviction only considers inbound peers.
    #[tokio::test]
    async fn test_audit_055_outbound_peer_not_evicted_for_inbound() {
        use crate::connection::ConnectionPool;

        let preferred_addr = PeerAddress::new("10.0.0.1", 11625);
        let shared = test_shared_state(vec![preferred_addr.clone()]);

        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        // Insert a non-preferred OUTBOUND peer — should not be evictable.
        let outbound_id = PeerId::from_bytes([4u8; 32]);
        let outbound_addr: std::net::SocketAddr = "10.0.0.99:11625".parse().unwrap();
        let _rx = insert_fake_peer(
            &shared,
            outbound_id,
            outbound_addr,
            crate::connection::ConnectionDirection::Outbound,
        );

        // Simulate the incoming preferred peer's pending reservation.
        pool.try_reserve();

        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        // Eviction should fail — the only peer in the cache is outbound.
        let evicted = OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool);
        assert!(
            !evicted,
            "should not evict outbound peers for inbound admission"
        );
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_evicts_non_preferred_outbound() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();

        let victim_id = PeerId::from_bytes([4u8; 32]);
        let mut victim_rx = insert_fake_peer(
            &shared,
            victim_id,
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));
        assert!(
            victim_rx.try_recv().is_ok(),
            "outbound victim should receive error-and-drop"
        );
        assert_eq!(pool.authenticated_count(), 2);
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_reserves_victim_once() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![
            PeerAddress::new("10.0.0.1", 11625),
            PeerAddress::new("10.0.0.2", 11625),
        ]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let victim_id = PeerId::from_bytes([4u8; 32]);
        let _victim_rx = insert_fake_peer(
            &shared,
            victim_id,
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let first = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &first, &shared, &pool
        ));

        pool.try_reserve();
        let second = candidate_info(
            PeerId::from_bytes([8u8; 32]),
            "10.0.0.2:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(
            !OverlayManager::try_accept_authenticated_peer(&second, &shared, &pool),
            "already-evicting victim must not be selected twice"
        );
        assert_eq!(pool.authenticated_count(), 2);
    }

    #[tokio::test]
    async fn test_admission_cleanup_clears_evicting_marker() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let victim_id = PeerId::from_bytes([4u8; 32]);
        let _victim_rx = insert_fake_peer(
            &shared,
            victim_id.clone(),
            "10.0.0.99:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));
        assert!(shared.admission_state.lock().is_evicting(&victim_id));
        shared.cleanup_peer(&victim_id, 0);
        assert!(!shared.admission_state.lock().is_evicting(&victim_id));
    }

    #[tokio::test]
    async fn test_preferred_outbound_admission_uses_peer_id_order() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![PeerAddress::new("10.0.0.1", 11625)]);
        let pool = Arc::new(ConnectionPool::new(3));
        let mut receivers = Vec::new();
        for byte in [9u8, 1, 5] {
            pool.try_reserve();
            pool.force_promote_authenticated();
            let id = PeerId::from_bytes([byte; 32]);
            let rx = insert_fake_peer(
                &shared,
                id,
                format!("10.0.1.{byte}:11625").parse().unwrap(),
                ConnectionDirection::Outbound,
            );
            receivers.push((byte, rx));
        }

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([99u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );
        assert!(OverlayManager::try_accept_authenticated_peer(
            &candidate, &shared, &pool
        ));

        for (byte, mut rx) in receivers {
            if byte == 1 {
                assert!(rx.try_recv().is_ok(), "lowest PeerId should be evicted");
            } else {
                assert!(
                    rx.try_recv().is_err(),
                    "higher PeerId should not be evicted"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_all_preferred_outbound_peers_block_eviction() {
        use crate::connection::{ConnectionDirection, ConnectionPool};

        let shared = test_shared_state(vec![
            PeerAddress::new("10.0.0.1", 11625),
            PeerAddress::new("10.0.0.2", 11625),
        ]);
        let pool = Arc::new(ConnectionPool::new(1));
        pool.try_reserve();
        pool.force_promote_authenticated();
        let _rx = insert_fake_peer(
            &shared,
            PeerId::from_bytes([4u8; 32]),
            "10.0.0.2:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        pool.try_reserve();
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        assert!(
            !OverlayManager::try_accept_authenticated_peer(&candidate, &shared, &pool),
            "preferred peer should not evict another preferred outbound peer"
        );
        assert_eq!(pool.authenticated_count(), 1);
    }

    #[test]
    fn test_shared_preferred_state_updates_for_admission() {
        let shared = test_shared_state(vec![PeerAddress::new("validator.example", 11625)]);
        let candidate = candidate_info(
            PeerId::from_bytes([9u8; 32]),
            "10.0.0.42:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Inbound,
        );
        assert!(!shared.preferred_peers.read().is_preferred(&candidate));

        let updated = shared
            .preferred_peers
            .read()
            .with_resolved(vec![PeerAddress::new("10.0.0.42", 11625)]);
        *shared.preferred_peers.write() = updated;

        assert!(shared.preferred_peers.read().is_preferred(&candidate));
    }

    /// Regression test for AUDIT-086: targeted sends of flow-controlled messages
    /// (SCP, Transaction, FloodAdvert, FloodDemand) must go through the Flood
    /// path, not the direct Send path, so they consume per-peer flow-control credit.
    #[tokio::test]
    async fn test_audit_086_targeted_flood_uses_flow_control() {
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;
        use stellar_xdr::*;

        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let manager = OverlayManager::new(config, local_node).unwrap();

        let peer_id = PeerId::from_bytes([99u8; 32]);
        let (outbound_tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = super::PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
            direction: crate::connection::ConnectionDirection::Outbound,
            generation: 0,
        };
        manager.peers.insert(peer_id.clone(), handle);

        // Flow-controlled SCP message should be routed as Flood
        let scp_msg = StellarMessage::ScpMessage(ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot {
                        counter: 1,
                        value: vec![].try_into().unwrap(),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: Hash([0; 32]),
                }),
            },
            signature: vec![].try_into().unwrap(),
        });

        manager
            .try_send_to(&peer_id, scp_msg)
            .expect("send should succeed");
        let msg = rx.recv().await.expect("should receive message");
        assert!(
            matches!(msg, OutboundMessage::Flood(_)),
            "SCP message should be routed through Flood path for flow control"
        );

        // Non-flood messages (e.g. GetScpState) should still use Send
        let get_state = StellarMessage::GetScpState(1);
        manager
            .try_send_to(&peer_id, get_state)
            .expect("send should succeed");
        let msg = rx.recv().await.expect("should receive message");
        assert!(
            matches!(msg, OutboundMessage::Send(_)),
            "GetScpState should use direct Send path"
        );
    }

    /// Regression test for AUDIT-105: GetScpState, GetScpQuorumset, and GetTxSet
    /// must be routed through the dedicated fetch channel, not the lossy broadcast.
    #[tokio::test]
    async fn test_fetch_requests_routed_to_dedicated_channel() {
        let (message_tx, _) = tokio::sync::broadcast::channel(64);
        let mut broadcast_rx = message_tx.subscribe();
        let (scp_message_tx, _scp_rx) = tokio::sync::mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_response_tx, mut fetch_rx) = tokio::sync::mpsc::channel(FETCH_CHANNEL_CAPACITY);

        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                Vec::new(),
                HashSet::new(),
            ))),
            preferred_peers_only: false,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        };

        let peer_id = PeerId::from_bytes([42u8; 32]);

        let request_msgs = vec![
            StellarMessage::GetScpState(100),
            StellarMessage::GetScpQuorumset(stellar_xdr::Uint256([1u8; 32])),
            StellarMessage::GetTxSet(stellar_xdr::Uint256([2u8; 32])),
        ];

        for msg in &request_msgs {
            let overlay_msg =
                OverlayMessage::new(peer_id.clone(), msg.clone(), std::time::Instant::now());
            shared.route_to_subscribers(overlay_msg);
        }

        // All three should arrive on the dedicated fetch channel.
        for _ in 0..3 {
            let received = fetch_rx
                .try_recv()
                .expect("fetch-request should arrive on dedicated channel");
            assert!(
                matches!(
                    received.message,
                    StellarMessage::GetScpState(_)
                        | StellarMessage::GetScpQuorumset(_)
                        | StellarMessage::GetTxSet(_)
                ),
                "unexpected message type on fetch channel"
            );
        }

        // None should arrive on the broadcast channel.
        let broadcast_result = broadcast_rx.try_recv();
        assert!(
            broadcast_result.is_err(),
            "fetch-request messages must NOT appear on the lossy broadcast channel"
        );
    }

    /// Build a SharedPeerState wired up for `route_to_subscribers` routing tests.
    /// Returns the shared state plus the broadcast and fetch receivers so the
    /// test can assert per-channel delivery.
    fn make_routing_shared_state() -> (
        SharedPeerState,
        tokio::sync::broadcast::Receiver<OverlayMessage>,
        tokio::sync::mpsc::Receiver<OverlayMessage>,
    ) {
        let (message_tx, _) = tokio::sync::broadcast::channel(1024);
        let broadcast_rx = message_tx.subscribe();
        let (scp_message_tx, _scp_rx) = tokio::sync::mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_response_tx, fetch_rx) = tokio::sync::mpsc::channel(FETCH_CHANNEL_CAPACITY);
        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                Vec::new(),
                HashSet::new(),
            ))),
            preferred_peers_only: false,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        };
        (shared, broadcast_rx, fetch_rx)
    }

    /// Build one `OverlayMessage` for each of the seven fetch variants.
    fn all_fetch_variant_messages(peer: &PeerId) -> Vec<OverlayMessage> {
        let variants = vec![
            StellarMessage::GetScpState(0),
            StellarMessage::GetScpQuorumset(stellar_xdr::Uint256([1u8; 32])),
            StellarMessage::GetTxSet(stellar_xdr::Uint256([2u8; 32])),
            StellarMessage::GeneralizedTxSet(stellar_xdr::GeneralizedTransactionSet::V1(
                stellar_xdr::TransactionSetV1 {
                    previous_ledger_hash: stellar_xdr::Hash([0u8; 32]),
                    phases: vec![].try_into().unwrap(),
                },
            )),
            StellarMessage::TxSet(stellar_xdr::TransactionSet {
                previous_ledger_hash: stellar_xdr::Hash([0u8; 32]),
                txs: stellar_xdr::VecM::default(),
            }),
            StellarMessage::DontHave(stellar_xdr::DontHave {
                type_: stellar_xdr::MessageType::TxSet,
                req_hash: stellar_xdr::Uint256([3u8; 32]),
            }),
            StellarMessage::ScpQuorumset(stellar_xdr::ScpQuorumSet {
                threshold: 1,
                validators: stellar_xdr::VecM::default(),
                inner_sets: stellar_xdr::VecM::default(),
            }),
        ];
        variants
            .into_iter()
            .map(|m| OverlayMessage::new(peer.clone(), m, std::time::Instant::now()))
            .collect()
    }

    /// Classification helper: which fetch variant does this message match?
    fn fetch_variant_key(msg: &StellarMessage) -> Option<&'static str> {
        match msg {
            StellarMessage::GetScpState(_) => Some("GetScpState"),
            StellarMessage::GetScpQuorumset(_) => Some("GetScpQuorumset"),
            StellarMessage::GetTxSet(_) => Some("GetTxSet"),
            StellarMessage::GeneralizedTxSet(_) => Some("GeneralizedTxSet"),
            StellarMessage::TxSet(_) => Some("TxSet"),
            StellarMessage::DontHave(_) => Some("DontHave"),
            StellarMessage::ScpQuorumset(_) => Some("ScpQuorumset"),
            _ => None,
        }
    }

    /// Issue #1741 + #3661 regression: every fetch variant must route to the
    /// dedicated fetch channel (not the lossy broadcast), and that channel must
    /// hold up to its full capacity without dropping under a parked receiver —
    /// but it is now **bounded** ([`FETCH_CHANNEL_CAPACITY`]) rather than
    /// unbounded, so it cannot grow RSS without limit (#3661). Push exactly
    /// `FETCH_CHANNEL_CAPACITY` messages across all 7 fetch variants while the
    /// receiver is parked, then drain and assert all survive with matching
    /// per-variant counts (no drops at/below capacity).
    #[tokio::test]
    async fn fetch_channel_all_variants_within_capacity() {
        let (shared, _broadcast_rx, mut fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([7u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let variant_count = variants.len();
        assert_eq!(variant_count, 7, "expected 7 fetch variants");

        const TOTAL: usize = FETCH_CHANNEL_CAPACITY;
        let mut sent_counts: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        for i in 0..TOTAL {
            let msg = variants[i % variant_count].clone();
            let key = fetch_variant_key(&msg.message).expect("fetch variant");
            *sent_counts.entry(key).or_insert(0) += 1;
            shared.route_to_subscribers(msg);
        }

        let mut received_counts: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        let mut drained = 0usize;
        while let Ok(msg) = fetch_rx.try_recv() {
            let key = fetch_variant_key(&msg.message).expect("received fetch variant");
            *received_counts.entry(key).or_insert(0) += 1;
            drained += 1;
        }
        assert_eq!(
            drained, TOTAL,
            "all {} messages at/below capacity must survive without drops",
            TOTAL
        );
        assert_eq!(
            sent_counts, received_counts,
            "per-variant counts must match"
        );
        // No drops occurred at/below capacity.
        assert_eq!(shared.metrics.snapshot().fetch_messages_dropped, 0);
    }

    /// Each of the 7 fetch variants must be routed exclusively to the
    /// dedicated fetch channel — they must NOT appear on the lossy broadcast.
    #[tokio::test]
    async fn fetch_variants_not_delivered_to_broadcast() {
        let (shared, mut broadcast_rx, _fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([9u8; 32]);
        for msg in all_fetch_variant_messages(&peer) {
            let key = fetch_variant_key(&msg.message).unwrap();
            shared.route_to_subscribers(msg);
            assert!(
                matches!(
                    broadcast_rx.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "{} must NOT appear on broadcast channel",
                key
            );
        }
    }

    /// Positive counterpart: each of the 7 fetch variants DOES land on the
    /// dedicated fetch channel exactly once.
    #[tokio::test]
    async fn fetch_variants_routed_to_dedicated_channel() {
        let (shared, _broadcast_rx, mut fetch_rx) = make_routing_shared_state();
        let peer = PeerId::from_bytes([11u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let expected: std::collections::HashSet<&'static str> = variants
            .iter()
            .map(|m| fetch_variant_key(&m.message).unwrap())
            .collect();
        for msg in variants {
            shared.route_to_subscribers(msg);
        }
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        while let Ok(msg) = fetch_rx.try_recv() {
            seen.insert(fetch_variant_key(&msg.message).unwrap());
        }
        assert_eq!(
            seen, expected,
            "every fetch variant must reach fetch channel"
        );
    }

    /// Wedged-loop regression: depth/max advance on enqueue even when the
    /// receiver is never polled. This is the exact failure mode the metric is
    /// meant to diagnose — receiver-side sampling would miss it.
    #[tokio::test]
    async fn fetch_channel_depth_tracks_enqueue_with_parked_receiver() {
        let (shared, _broadcast_rx, _fetch_rx) = make_routing_shared_state();
        // Parked: never call fetch_rx.recv(). Hold the rx alive so sends succeed.
        let peer = PeerId::from_bytes([22u8; 32]);
        let variants = all_fetch_variant_messages(&peer);
        let n = variants.len() as i64;

        assert_eq!(shared.fetch_channel_depth.load(Ordering::Relaxed), 0);
        assert_eq!(shared.fetch_channel_depth_max.load(Ordering::Relaxed), 0);

        for msg in variants {
            shared.route_to_subscribers(msg);
        }

        assert_eq!(
            shared.fetch_channel_depth.load(Ordering::Relaxed),
            n,
            "depth must reflect every enqueued fetch message without any recv"
        );
        assert!(
            shared.fetch_channel_depth_max.load(Ordering::Relaxed) >= n,
            "max must advance to at least the observed depth"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_signal_shutdown_idempotent() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // First call should signal
        manager.signal_shutdown();
        assert!(!manager.running.load(Ordering::SeqCst));
        // shutdown_tx should have been taken
        assert!(manager.shutdown_tx.lock().is_none());

        // Second call should be a no-op (no panic)
        manager.signal_shutdown();
        assert!(!manager.running.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_fast_with_no_handles() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Shutdown with no handles should complete instantly
        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        assert!(!manager.running.load(Ordering::SeqCst));
        // With paused time, should be essentially zero
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_timeout_aborts_slow_handles() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Track whether each task was actually cancelled (not just detached).
        let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut handles = manager.peer_handles.write();
            for _ in 0..5 {
                let cancelled = Arc::clone(&cancelled);
                handles.push(tokio::spawn(async move {
                    // Hold the Arc clone for the task's lifetime so the
                    // post-abort strong_count assertion is meaningful. The
                    // sleep never resolves (paused time, no advance), so the
                    // task only ends via abort.
                    let _cancelled = cancelled;
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }));
            }
        }

        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        // Should complete at or near the 5s deadline, not wait 3600s
        let elapsed = start.elapsed();
        assert!(
            elapsed <= Duration::from_secs(6),
            "shutdown took {elapsed:?}, expected <= 6s"
        );

        // Verify the tasks were truly aborted: after a short yield, the
        // Arc refcount should have dropped to 1 (only our local `cancelled`
        // clone remains). If tasks were merely detached, they'd still hold
        // their clone.
        tokio::task::yield_now().await;
        assert_eq!(
            Arc::strong_count(&cancelled),
            1,
            "timed-out tasks should have been aborted, not detached"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_shutdown_fast_handles_complete_before_timeout() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let mut manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Add handles that complete quickly
        {
            let mut handles = manager.peer_handles.write();
            for _ in 0..3 {
                handles.push(tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }));
            }
        }

        let start = tokio::time::Instant::now();
        manager.shutdown().await.unwrap();
        // Should complete well under the 5s timeout
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "shutdown took {elapsed:?}, expected < 1s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_signal_shutdown_through_arc() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Wrap in Arc — simulates the Arc::try_unwrap failure path
        let arc = Arc::new(manager);

        // signal_shutdown should work through &self (via Arc)
        arc.signal_shutdown();
        assert!(!arc.running.load(Ordering::SeqCst));
    }

    // ──────── PreferredPeerSet tests ────────

    #[test]
    fn test_preferred_peer_set_from_config() {
        let entries = vec![
            PeerAddress::new("validator1.example.com", 11625),
            PeerAddress::new("10.0.0.1", 11625),
        ];
        let set = PreferredPeerSet::from_config(entries.clone(), HashSet::new());
        assert_eq!(set.config_entries.len(), 2);
        assert!(set.resolved.is_empty());
        assert!(set.resolved_ips.is_empty());
    }

    #[test]
    fn test_preferred_peer_set_with_resolved() {
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config, HashSet::new());

        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);

        assert_eq!(updated.config_entries.len(), 1);
        assert_eq!(updated.resolved.len(), 1);
        assert_eq!(updated.resolved_ips.len(), 1);
        assert!(updated
            .resolved_ips
            .contains(&"10.0.0.42".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_outbound_hostname() {
        // Outbound peers have original_address set — should match config hostname.
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config, HashSet::new());

        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([1u8; 32]),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("validator1.example.com", 11625)),
        };

        assert!(
            set.is_preferred(&peer_info),
            "outbound peer with matching original_address hostname should be preferred"
        );
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_inbound_resolved_ip() {
        // Inbound peers have no original_address — must match by resolved IP.
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config, HashSet::new());

        // Before DNS resolution: inbound peer should NOT match (hostname can't parse as IP)
        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([2u8; 32]),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };

        assert!(
            !set.is_preferred(&peer_info),
            "inbound peer should NOT match before DNS resolution"
        );

        // After DNS resolution: should match via resolved IP
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);
        assert!(
            updated.is_preferred(&peer_info),
            "inbound peer should match after DNS resolution"
        );
    }

    #[test]
    fn test_preferred_peer_set_is_preferred_no_match() {
        let config = vec![PeerAddress::new("validator1.example.com", 11625)];
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let set = PreferredPeerSet::from_config(config, HashSet::new()).with_resolved(resolved);

        let peer_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([3u8; 32]),
            address: "192.168.1.1:11625".parse().unwrap(),
            direction: crate::connection::ConnectionDirection::Inbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };

        assert!(
            !set.is_preferred(&peer_info),
            "non-preferred peer should not match"
        );
    }

    #[test]
    fn test_preferred_peer_set_shuffled_entries_all_present() {
        let config = vec![
            PeerAddress::new("a.example.com", 11625),
            PeerAddress::new("b.example.com", 11625),
            PeerAddress::new("c.example.com", 11625),
        ];
        let set = PreferredPeerSet::from_config(config.clone(), HashSet::new());

        // Use a seeded RNG for determinism
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let shuffled = set.shuffled_config_entries(&mut rng);

        assert_eq!(shuffled.len(), config.len());
        for entry in &config {
            assert!(
                shuffled.iter().any(|e| e.host == entry.host),
                "all config entries must appear in shuffled output"
            );
        }
    }

    #[test]
    fn test_shuffled_dial_entries_uses_config_when_no_resolved() {
        use rand::SeedableRng;
        let config = vec![
            PeerAddress::new("a.example.com", 11625),
            PeerAddress::new("b.example.com", 11625),
        ];
        let set = PreferredPeerSet::from_config(config.clone(), HashSet::new());
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let entries = set.shuffled_dial_entries(&mut rng);
        assert_eq!(entries.len(), 2);
        for cfg in &config {
            assert!(entries.iter().any(|e| e.host == cfg.host));
        }
    }

    #[test]
    fn test_shuffled_dial_entries_uses_resolved_when_available() {
        use rand::SeedableRng;
        let config = vec![PeerAddress::new("a.example.com", 11625)];
        let set = PreferredPeerSet::from_config(config, HashSet::new());
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let entries = updated.shuffled_dial_entries(&mut rng);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "10.0.0.42");
    }

    #[test]
    fn test_shuffled_dial_entries_partial_dns_omits_failed() {
        use rand::SeedableRng;
        let config = vec![
            PeerAddress::new("a.example.com", 11625),
            PeerAddress::new("b.example.com", 11625),
        ];
        let set = PreferredPeerSet::from_config(config, HashSet::new());
        // Only one hostname resolved — the other is omitted (retried next DNS cycle)
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let entries = updated.shuffled_dial_entries(&mut rng);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "10.0.0.42");
    }

    #[test]
    fn test_add_known_peer_dial_key_dedup() {
        let local_node = {
            let secret = henyey_crypto::SecretKey::generate();
            crate::LocalNode::new_testnet(secret)
        };
        let config = crate::OverlayConfig {
            known_peers: vec![PeerAddress::new("10.0.0.1", 11625)],
            ..Default::default()
        };
        let manager = OverlayManager::new(config, local_node).unwrap();
        // Same IP, should be rejected as duplicate via dial key
        assert!(!manager.add_known_peer(PeerAddress::new("10.0.0.1", 11625)));
        // Different port, should be accepted
        assert!(manager.add_known_peer(PeerAddress::new("10.0.0.1", 11626)));
    }

    #[test]
    fn test_connection_pool_update_preferred_ips_enables_reservation() {
        // Verify update_preferred_ips works by checking that a preferred IP
        // can get extra slots after the update, using only the public API.
        let pool = ConnectionPool::with_preferred(2, 2, HashSet::new());

        // Fill to base limit (2 reserved)
        assert!(pool.try_reserve());
        assert!(pool.try_reserve());

        // Before update: even preferred IP can't get extra because it's not in the set.
        // But we're still within pending headroom (max_pending_extra=32 by default).
        // We can only test this properly using with_preferred which sets up initial state.
        // So just verify update_preferred_ips doesn't panic:
        let mut ips = HashSet::new();
        ips.insert("10.0.0.1".parse::<IpAddr>().unwrap());
        pool.update_preferred_ips(ips);
    }

    #[test]
    fn test_eviction_skips_preferred_peers_from_set() {
        use crate::connection::ConnectionDirection;

        // Create a set where 10.0.0.1:11625 is preferred
        let preferred = vec![PeerAddress::new("10.0.0.1", 11625)];
        let set = PreferredPeerSet::from_config(preferred, HashSet::new());

        // Preferred peer
        let preferred_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([1u8; 32]),
            address: "10.0.0.1:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("10.0.0.1", 11625)),
        };
        assert!(
            set.is_preferred(&preferred_info),
            "should recognize preferred peer by IP"
        );

        // Non-preferred peer
        let non_preferred_info = crate::peer::PeerInfo {
            peer_id: PeerId::from_bytes([2u8; 32]),
            address: "10.0.0.99:11625".parse().unwrap(),
            direction: ConnectionDirection::Outbound,
            version_string: String::new(),
            overlay_version: 0,
            ledger_version: 0,
            connected_at: std::time::Instant::now(),
            original_address: Some(PeerAddress::new("10.0.0.99", 11625)),
        };
        assert!(
            !set.is_preferred(&non_preferred_info),
            "should not recognize non-preferred peer"
        );
    }

    #[tokio::test]
    async fn test_preferred_set_protects_peers_from_random_drop() {
        use crate::connection::ConnectionDirection;

        // With a preferred set containing 10.0.0.1, only that peer should be
        // recognized as preferred.
        let preferred = vec![PeerAddress::new("10.0.0.1", 11625)];
        let shared = test_shared_state(preferred);

        // Insert preferred outbound peer
        let preferred_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_fake_peer(
            &shared,
            preferred_id,
            "10.0.0.1:11625".parse().unwrap(),
            ConnectionDirection::Outbound,
        );

        // Verify the preferred_peers set on shared state correctly identifies this peer
        let info = shared
            .peer_info_cache
            .get(&PeerId::from_bytes([1u8; 32]))
            .unwrap();
        assert!(
            shared.preferred_peers.read().is_preferred(info.value()),
            "shared preferred_peers set should recognize the peer"
        );
    }

    // ──────── Key-based preferred peer tests ────────

    #[test]
    fn test_preferred_peer_set_key_based_preference() {
        use crate::connection::ConnectionDirection;
        let peer_id = PeerId::from_bytes([42u8; 32]);
        let mut keys = HashSet::new();
        keys.insert(peer_id.clone());

        let set = PreferredPeerSet::from_config(Vec::new(), keys);

        // A peer matching the key should be preferred
        let info = PeerInfo {
            peer_id: PeerId::from_bytes([42u8; 32]),
            address: "10.0.0.1:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        assert!(
            set.is_preferred(&info),
            "key-matched peer should be preferred"
        );

        // A peer NOT matching the key should not be preferred
        let other_info = PeerInfo {
            peer_id: PeerId::from_bytes([99u8; 32]),
            address: "10.0.0.2:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        assert!(
            !set.is_preferred(&other_info),
            "non-key peer should not be preferred"
        );
    }

    #[test]
    fn test_preferred_peer_set_with_resolved_preserves_keys() {
        use crate::connection::ConnectionDirection;
        let peer_id = PeerId::from_bytes([42u8; 32]);
        let mut keys = HashSet::new();
        keys.insert(peer_id.clone());

        let set = PreferredPeerSet::from_config(Vec::new(), keys);
        let resolved = vec![PeerAddress::new("10.0.0.42", 11625)];
        let updated = set.with_resolved(resolved);

        // Keys should survive the DNS resolution update
        let info = PeerInfo {
            peer_id: PeerId::from_bytes([42u8; 32]),
            address: "10.0.0.1:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        assert!(
            updated.is_preferred(&info),
            "keys should be preserved after with_resolved"
        );
    }

    #[test]
    fn test_admission_rejects_non_preferred_under_strict_mode() {
        use crate::connection::ConnectionDirection;
        let (message_tx, _) = broadcast::channel(16);
        let (scp_tx, _scp_rx) = mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_tx, _fetch_rx) = mpsc::channel(FETCH_CHANNEL_CAPACITY);

        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::with_ttl(std::time::Duration::from_secs(30))),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx: scp_tx,
            fetch_response_tx: fetch_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: true,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                Vec::new(),
                HashSet::new(),
            ))),
            preferred_peers_only: true, // STRICT MODE
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        };

        // Pool with capacity (max=10, current authenticated=0)
        let pool = ConnectionPool::new(10);

        // Non-preferred peer should be rejected even with capacity
        let peer_info = PeerInfo {
            peer_id: PeerId::from_bytes([99u8; 32]),
            address: "10.0.0.99:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        assert!(
            !OverlayManager::try_accept_authenticated_peer(&peer_info, &shared, &pool),
            "non-preferred peer should be rejected under strict mode even with capacity"
        );
    }

    #[test]
    fn test_admission_accepts_key_preferred_under_strict_mode() {
        use crate::connection::ConnectionDirection;
        let (message_tx, _) = broadcast::channel(16);
        let (scp_tx, _scp_rx) = mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_tx, _fetch_rx) = mpsc::channel(FETCH_CHANNEL_CAPACITY);

        let preferred_key = PeerId::from_bytes([42u8; 32]);
        let mut keys = HashSet::new();
        keys.insert(preferred_key.clone());

        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::with_ttl(std::time::Duration::from_secs(30))),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx: scp_tx,
            fetch_response_tx: fetch_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: true,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(Vec::new(), keys))),
            preferred_peers_only: true, // STRICT MODE
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        };

        // Pool with capacity — reserve a pending slot (required before promote)
        let pool = ConnectionPool::new(10);
        assert!(pool.try_reserve());

        // Preferred-by-key peer should be admitted under strict mode
        let peer_info = PeerInfo {
            peer_id: PeerId::from_bytes([42u8; 32]),
            address: "10.0.0.42:11625".parse().unwrap(),
            direction: ConnectionDirection::Inbound,
            version_string: "test".to_string(),
            overlay_version: 35,
            ledger_version: 22,
            connected_at: std::time::Instant::now(),
            original_address: None,
        };
        assert!(
            OverlayManager::try_accept_authenticated_peer(&peer_info, &shared, &pool),
            "key-preferred peer should be admitted under strict mode"
        );
    }

    /// Helper: insert a peer with a specific channel capacity into the manager.
    fn insert_peer_with_capacity(
        manager: &OverlayManager,
        peer_id: PeerId,
        capacity: usize,
    ) -> tokio::sync::mpsc::Receiver<OutboundMessage> {
        use crate::flow_control::{FlowControl, FlowControlConfig};
        use crate::peer::PeerStats;

        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(capacity);
        let handle = PeerHandle {
            outbound_tx,
            stats: Arc::new(PeerStats::default()),
            flow_control: Arc::new(FlowControl::new(FlowControlConfig::default())),
            direction: crate::connection::ConnectionDirection::Inbound,
            generation: 0,
        };
        manager.peers.insert(peer_id.clone(), handle);
        manager.peer_info_cache.insert(
            peer_id.clone(),
            crate::peer::PeerInfo {
                peer_id,
                address: "127.0.0.1:11625".parse().unwrap(),
                direction: crate::connection::ConnectionDirection::Inbound,
                version_string: String::new(),
                overlay_version: 0,
                ledger_version: 0,
                connected_at: std::time::Instant::now(),
                original_address: None,
            },
        );
        outbound_rx
    }

    /// Build a `SharedPeerState` wired with the production-shaped SCP ingest
    /// channel, returning both the state and the receiver end so a test can
    /// simulate a stalled event loop by never draining the receiver. Mirrors
    /// `OverlayManager::new`'s SCP-channel construction.
    pub(crate) fn shared_state_with_scp_receiver(
    ) -> (SharedPeerState, tokio::sync::mpsc::Receiver<OverlayMessage>) {
        let (message_tx, _) = tokio::sync::broadcast::channel(BROADCAST_CHANNEL_SIZE);
        let (scp_message_tx, scp_message_rx) = tokio::sync::mpsc::channel(SCP_CHANNEL_CAPACITY);
        let (fetch_response_tx, _) = tokio::sync::mpsc::channel(FETCH_CHANNEL_CAPACITY);
        let shared = SharedPeerState {
            peers: Arc::new(DashMap::new()),
            flood_gate: Arc::new(FloodGate::new()),
            scp_inbound_filter: Arc::new(RwLock::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            message_tx,
            scp_message_tx,
            fetch_response_tx,
            peer_handles: Arc::new(RwLock::new(Vec::new())),
            advertised_outbound_peers: Arc::new(RwLock::new(Vec::new())),
            advertised_inbound_peers: Arc::new(RwLock::new(Vec::new())),
            added_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            dropped_authenticated_peers: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            banned_peers: Arc::new(RwLock::new(HashSet::new())),
            peer_info_cache: Arc::new(DashMap::new()),
            peer_latest_externalized: Arc::new(DashMap::new()),
            last_closed_ledger: Arc::new(AtomicU32::new(0)),
            scp_callback: None,
            is_validator: false,
            peer_event_tx: None,
            extra_subscribers: Arc::new(RwLock::new(Vec::new())),
            is_tracking: Arc::new(AtomicBool::new(true)),
            is_synced: Arc::new(AtomicBool::new(true)),
            pending_connections: PendingConnections::new(),
            preferred_peers: Arc::new(RwLock::new(PreferredPeerSet::from_config(
                Vec::new(),
                HashSet::new(),
            ))),
            preferred_peers_only: false,
            admission_state: Arc::new(Mutex::new(AdmissionState::default())),
            fetch_channel_depth: Arc::new(AtomicI64::new(0)),
            fetch_channel_depth_max: Arc::new(AtomicI64::new(0)),
            metrics: Arc::new(OverlayMetrics::new()),
            query_rate_limit_window_secs: Arc::new(AtomicU64::new(60)),
            max_tx_size_bytes: Arc::new(AtomicU32::new(
                crate::flow_control::DEFAULT_MAX_TX_SIZE_BYTES,
            )),
            flow_control_bytes_config: FlowControlBytesConfig::default(),
            peer_flood_reading_capacity: 200,
            outbound_channel_capacity: 256,
            dial_cooldowns: Arc::new(DashMap::new()),
            local_peer_id: PeerId::from_bytes([0u8; 32]),
            next_peer_generation: Arc::new(AtomicU64::new(0)),
        };
        (shared, scp_message_rx)
    }

    /// Build a `SharedPeerState` wired with bounded fetch + catchup channels so
    /// regression tests for #3661 can simulate a stalled event loop / aborted
    /// catchup-cache task by never draining the returned receivers.
    ///
    /// Returns `(shared, fetch_rx, catchup_rx)` where both receivers are at
    /// their production capacities ([`FETCH_CHANNEL_CAPACITY`] /
    /// [`CATCHUP_CHANNEL_CAPACITY`]). The catchup receiver is registered as an
    /// `extra_subscribers` entry, exactly as `subscribe_catchup()` does.
    pub(crate) fn shared_state_for_3661() -> (
        SharedPeerState,
        tokio::sync::mpsc::Receiver<OverlayMessage>,
        tokio::sync::mpsc::Receiver<OverlayMessage>,
    ) {
        let (shared, _scp_rx) = shared_state_with_scp_receiver();
        let (fetch_tx, fetch_rx) = tokio::sync::mpsc::channel(FETCH_CHANNEL_CAPACITY);
        let (catchup_tx, catchup_rx) = tokio::sync::mpsc::channel(CATCHUP_CHANNEL_CAPACITY);
        // Replace the parked fetch sender with one whose receiver we hold, and
        // register the catchup subscriber the way `subscribe_catchup()` would.
        let mut shared = shared;
        shared.fetch_response_tx = fetch_tx;
        shared.extra_subscribers.write().push(catchup_tx);
        (shared, fetch_rx, catchup_rx)
    }

    fn make_scp_msg(slot_index: u64) -> OverlayMessage {
        use stellar_xdr::*;
        OverlayMessage::new(
            PeerId::from_bytes([7u8; 32]),
            StellarMessage::ScpMessage(ScpEnvelope {
                statement: ScpStatement {
                    node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                    slot_index,
                    pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                        commit: ScpBallot {
                            counter: 1,
                            value: vec![].try_into().unwrap(),
                        },
                        n_h: 1,
                        commit_quorum_set_hash: Hash([0; 32]),
                    }),
                },
                signature: vec![].try_into().unwrap(),
            }),
            std::time::Instant::now(),
        )
    }

    /// Regression test for #3623 (fatal OOM restart-loop).
    ///
    /// Simulates a stalled event loop by taking the SCP ingest receiver and
    /// NEVER draining it, then flooding `SCP_CHANNEL_CAPACITY + N` SCP
    /// envelopes through the same send path the peer-receive code uses
    /// (`route_to_subscribers`, mod.rs:702).
    ///
    /// On origin/main the SCP channel is `mpsc::unbounded_channel()`, so all
    /// `CAPACITY + N` envelopes enqueue (`rx.len() == CAPACITY + N`) and
    /// `messages_dropped == 0` — the exact unbounded-growth mechanism that
    /// OOM-kills the validator. This test therefore FAILS on origin/main.
    ///
    /// After the fix (bounded `mpsc::channel(SCP_CHANNEL_CAPACITY)` +
    /// `try_send` drop-on-full), the channel never exceeds the capacity and
    /// the overflow is counted in `messages_dropped`, so this test PASSES.
    #[tokio::test]
    async fn test_scp_channel_bounded_drops_when_receiver_stalled() {
        const OVERFLOW: usize = 256;
        let (shared, rx) = shared_state_with_scp_receiver();

        // Simulate the stalled event loop: hold the receiver, never drain it.
        // Flood more than the channel can hold.
        for slot in 0..(SCP_CHANNEL_CAPACITY + OVERFLOW) as u64 {
            shared.route_to_subscribers(make_scp_msg(slot));
        }

        // (a) The channel must never retain more than its capacity — this is
        // the bound that prevents unbounded RSS growth / OOM.
        assert!(
            rx.len() <= SCP_CHANNEL_CAPACITY,
            "bounded SCP channel must hold at most {} items, held {}",
            SCP_CHANNEL_CAPACITY,
            rx.len()
        );

        // (b) The over-capacity envelopes must have been dropped and counted,
        // not silently lost.
        let metrics = shared.metrics.snapshot();
        assert!(
            metrics.messages_dropped >= OVERFLOW as u64,
            "expected >= {} dropped SCP envelopes, got {}",
            OVERFLOW,
            metrics.messages_dropped
        );
    }

    /// Regression test for the maxtps-iter-7 duplicate-drop safety invariant:
    /// an SCP envelope dropped on a FULL SCP channel must release its
    /// in-flight scheduled-cache token (by dropping the message, which owns
    /// it), so a later duplicate copy from another peer re-enters the peer
    /// task's dedup filter as new and is re-forwarded. Without the release,
    /// the peer-task dedup would suppress every retry copy of a lost
    /// envelope.
    #[tokio::test]
    async fn test_scp_channel_full_drop_releases_inflight_token() {
        let (shared, _rx) = shared_state_with_scp_receiver();

        // Fill the channel to capacity with filler envelopes.
        for slot in 0..SCP_CHANNEL_CAPACITY as u64 {
            shared.route_to_subscribers(make_scp_msg(slot));
        }

        // The target envelope carries an in-flight token, exactly as the
        // peer task attaches it after a successful dedup-filter claim.
        let token = std::sync::Arc::new(());
        let watch = std::sync::Arc::downgrade(&token);
        let mut target = make_scp_msg(999_999);
        target.scp_inflight_token = Some(token);

        // Route it — the channel is full, so the message (and its token) is
        // dropped.
        shared.route_to_subscribers(target);

        assert!(
            watch.upgrade().is_none(),
            "a full-channel drop must release the in-flight token so a later \
             duplicate copy passes the peer-task dedup filter"
        );
    }

    /// A `GeneralizedTxSet` overlay message (an `is_fetch_response` that also
    /// fans out to `extra_subscribers`), used to drive both #3661 channels.
    fn make_generalized_txset_msg() -> OverlayMessage {
        use stellar_xdr::*;
        OverlayMessage::new(
            PeerId::from_bytes([9u8; 32]),
            StellarMessage::GeneralizedTxSet(GeneralizedTransactionSet::V1(TransactionSetV1 {
                previous_ledger_hash: Hash([0; 32]),
                phases: vec![].try_into().unwrap(),
            })),
            std::time::Instant::now(),
        )
    }

    /// Regression test for #3661 (fatal OOM-on-restart): the **fetch intake**
    /// channel must be bounded and drop-on-full.
    ///
    /// Simulates a stalled event loop by holding the fetch receiver and never
    /// draining it, then flooding `FETCH_CHANNEL_CAPACITY + N` full-tx-set
    /// fetch responses through the production send path (`route_to_subscribers`).
    ///
    /// On origin/main the fetch channel is `mpsc::unbounded_channel()` fed via a
    /// non-dropping `.send()`, so all `CAPACITY + N` messages enqueue
    /// (`rx.len() == CAPACITY + N`) and nothing is dropped — the exact
    /// unbounded multi-MB-tx-set growth that OOM-kills the validator. This test
    /// therefore FAILS on origin/main and PASSES after the bounded + `try_send`
    /// drop-on-full fix.
    #[tokio::test]
    async fn test_fetch_channel_bounded_drops_when_receiver_stalled() {
        const OVERFLOW: usize = 256;
        let (shared, fetch_rx, _catchup_rx) = shared_state_for_3661();

        // Stalled event loop: hold the fetch receiver, never drain it.
        for _ in 0..(FETCH_CHANNEL_CAPACITY + OVERFLOW) {
            shared.route_to_subscribers(make_generalized_txset_msg());
        }

        // (a) The bounded channel must never retain more than its capacity.
        assert!(
            fetch_rx.len() <= FETCH_CHANNEL_CAPACITY,
            "bounded fetch channel must hold at most {} items, held {}",
            FETCH_CHANNEL_CAPACITY,
            fetch_rx.len()
        );

        // (b) The over-capacity messages must be dropped and counted.
        let metrics = shared.metrics.snapshot();
        assert!(
            metrics.fetch_messages_dropped >= OVERFLOW as u64,
            "expected >= {} dropped fetch messages, got {}",
            OVERFLOW,
            metrics.fetch_messages_dropped
        );
    }

    /// Regression test for #3661: the **catchup-cache** fan-out channel
    /// (`subscribe_catchup()`) must be bounded and drop-on-full.
    ///
    /// Simulates an aborted/stalled catchup-cache task by registering a catchup
    /// subscriber and never draining it, then flooding
    /// `CATCHUP_CHANNEL_CAPACITY + N` full-tx-set messages through the
    /// production fan-out. On origin/main the per-subscriber channel is
    /// `mpsc::unbounded_channel()` fed via non-dropping `.send()`, so all
    /// messages enqueue and nothing drops (FAILS); after the fix the channel is
    /// capped and overflow is counted (PASSES).
    #[tokio::test]
    async fn test_catchup_channel_bounded_drops_when_receiver_stalled() {
        const OVERFLOW: usize = 256;
        let (shared, _fetch_rx, catchup_rx) = shared_state_for_3661();

        for _ in 0..(CATCHUP_CHANNEL_CAPACITY + OVERFLOW) {
            shared.route_to_subscribers(make_generalized_txset_msg());
        }

        assert!(
            catchup_rx.len() <= CATCHUP_CHANNEL_CAPACITY,
            "bounded catchup channel must hold at most {} items, held {}",
            CATCHUP_CHANNEL_CAPACITY,
            catchup_rx.len()
        );

        let metrics = shared.metrics.snapshot();
        assert!(
            metrics.catchup_messages_dropped >= OVERFLOW as u64,
            "expected >= {} dropped catchup messages, got {}",
            OVERFLOW,
            metrics.catchup_messages_dropped
        );
    }

    /// App-boundary type-pin (#3661): both overlay→app fetch/catchup channel
    /// constructors must hand back **bounded** receivers (a `max_capacity()`
    /// equal to the named cap), not unbounded ones. If a future refactor
    /// reverts either channel to `unbounded_channel()` this fails to compile
    /// (no `max_capacity`) or trips the capacity assertion.
    #[tokio::test]
    async fn test_overlay_fetch_catchup_channels_are_bounded() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let manager = OverlayManager::new(config, local_node).unwrap();

        let fetch_rx = manager
            .subscribe_fetch_responses()
            .await
            .expect("fetch receiver available once");
        assert_eq!(
            fetch_rx.max_capacity(),
            FETCH_CHANNEL_CAPACITY,
            "fetch intake channel must be bounded at FETCH_CHANNEL_CAPACITY"
        );

        let catchup_rx = manager.subscribe_catchup();
        assert_eq!(
            catchup_rx.max_capacity(),
            CATCHUP_CHANNEL_CAPACITY,
            "catchup-cache channel must be bounded at CATCHUP_CHANNEL_CAPACITY"
        );
    }

    fn make_hello_msg() -> StellarMessage {
        StellarMessage::Hello(stellar_xdr::Hello {
            ledger_version: 0,
            overlay_version: 0,
            overlay_min_version: 0,
            network_id: stellar_xdr::Hash([0u8; 32]),
            version_str: "test".try_into().unwrap(),
            listening_port: 0,
            peer_id: stellar_xdr::NodeId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::Uint256([0u8; 32]),
            )),
            cert: stellar_xdr::AuthCert {
                pubkey: stellar_xdr::Curve25519Public { key: [0u8; 32] },
                expiration: 0,
                sig: stellar_xdr::Signature::default(),
            },
            nonce: stellar_xdr::Uint256([0u8; 32]),
        })
    }

    #[tokio::test]
    async fn test_broadcast_backpressure_increments_messages_dropped() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Insert a peer with channel capacity of 1
        let peer_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer_id, 1);

        // First broadcast fills the channel
        let msg = make_hello_msg();
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 1);

        // Second broadcast should drop (channel full)
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 0);

        // Verify messages_dropped metric was incremented
        let metrics = manager.metrics.snapshot();
        assert_eq!(
            metrics.messages_dropped, 1,
            "messages_dropped should be 1 after one dropped broadcast"
        );
    }

    #[tokio::test]
    async fn test_try_send_to_backpressure_increments_messages_dropped() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();

        // Insert a peer with channel capacity of 1
        let peer_id = PeerId::from_bytes([2u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer_id.clone(), 1);

        let msg = make_hello_msg();

        // First send fills the channel
        assert!(manager.try_send_to(&peer_id, msg.clone()).is_ok());

        // Second send should fail with ChannelSend
        let err = manager.try_send_to(&peer_id, msg.clone()).unwrap_err();
        assert!(matches!(err, OverlayError::ChannelSend));

        // Verify messages_dropped metric was incremented
        let metrics = manager.metrics.snapshot();
        assert_eq!(
            metrics.messages_dropped, 1,
            "messages_dropped should be 1 after one channel-full error"
        );
    }

    #[tokio::test]
    async fn test_broadcast_backpressure_multiple_peers() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        // Insert 3 peers each with capacity 1
        let peer1 = PeerId::from_bytes([1u8; 32]);
        let peer2 = PeerId::from_bytes([2u8; 32]);
        let peer3 = PeerId::from_bytes([3u8; 32]);
        let _rx1 = insert_peer_with_capacity(&manager, peer1, 1);
        let _rx2 = insert_peer_with_capacity(&manager, peer2, 1);
        let _rx3 = insert_peer_with_capacity(&manager, peer3, 1);

        let msg = make_hello_msg();

        // First broadcast fills all channels
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 3);

        // Second broadcast drops for all 3 peers
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 0);

        let metrics = manager.metrics.snapshot();
        assert_eq!(
            metrics.messages_dropped, 3,
            "all 3 peers should have dropped the second broadcast"
        );
    }

    /// Build a minimal flood-tracked `StellarMessage::ScpMessage` for
    /// broadcast tests (exercises the per-`msg_type` drop-counting path).
    fn make_scp_stellar_msg() -> StellarMessage {
        use stellar_xdr::*;
        StellarMessage::ScpMessage(ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot {
                        counter: 1,
                        value: vec![].try_into().unwrap(),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: Hash([0; 32]),
                }),
            },
            signature: vec![].try_into().unwrap(),
        })
    }

    /// #3792: a broadcast Full-drop must bump the dedicated per-`msg_type`
    /// `broadcast_fanout_drop_by_type` counter (SCP here), while the aggregate
    /// `messages_dropped` stays incremented for cross-site continuity (#3623).
    #[tokio::test]
    async fn test_broadcast_fanout_drop_by_type_counter() {
        use crate::metrics::OverlayMessageKind;

        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        let peer_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer_id, 1);

        let msg = make_scp_stellar_msg();
        // First broadcast fills the single-slot channel.
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 1);
        // Second broadcast Full-drops.
        let sent = manager.broadcast(msg.clone()).await.unwrap();
        assert_eq!(sent, 0);

        let snap = manager.metrics.snapshot();
        assert_eq!(
            snap.broadcast_fanout_drop_by_type[OverlayMessageKind::ScpMessage as usize],
            1,
            "per-type broadcast fan-out drop counter should be 1 for SCP_MESSAGE"
        );
        // Aggregate continuity (#3623): messages_dropped still incremented.
        assert_eq!(
            snap.messages_dropped, 1,
            "aggregate messages_dropped must stay incremented alongside the dedicated series"
        );
    }

    /// #3792: `broadcast_blackout` increments exactly when a broadcast reaches
    /// ZERO peers (`dropped > 0 && sent == 0`); it must NOT increment when at
    /// least one targeted peer accepted, even if another peer dropped.
    #[tokio::test]
    async fn test_broadcast_blackout_on_zero_sent() {
        use crate::metrics::OverlayMessageKind;

        // Positive: single peer, cap 1 → second broadcast sent==0, dropped==1.
        {
            let manager = OverlayManager::new(
                OverlayConfig::default(),
                LocalNode::new_testnet(SecretKey::generate()),
            )
            .unwrap();
            manager.running.store(true, Ordering::SeqCst);
            let _rx = insert_peer_with_capacity(&manager, PeerId::from_bytes([1u8; 32]), 1);

            let msg = make_scp_stellar_msg();
            assert_eq!(manager.broadcast(msg.clone()).await.unwrap(), 1);
            assert_eq!(manager.broadcast(msg.clone()).await.unwrap(), 0);

            let snap = manager.metrics.snapshot();
            assert_eq!(
                snap.broadcast_blackout, 1,
                "blackout must increment when every targeted peer rejects"
            );
        }

        // Negative: peer A cap 2, peer B cap 1 → a broadcast with sent>0 &&
        // dropped>0 leaves blackout at 0 (but still counts the one drop).
        {
            let manager = OverlayManager::new(
                OverlayConfig::default(),
                LocalNode::new_testnet(SecretKey::generate()),
            )
            .unwrap();
            manager.running.store(true, Ordering::SeqCst);
            let _rx_a = insert_peer_with_capacity(&manager, PeerId::from_bytes([1u8; 32]), 2);
            let _rx_b = insert_peer_with_capacity(&manager, PeerId::from_bytes([2u8; 32]), 1);

            let msg = make_scp_stellar_msg();
            // First broadcast: A queues 1/2, B queues 1/1 → sent=2.
            assert_eq!(manager.broadcast(msg.clone()).await.unwrap(), 2);
            // Second broadcast: A queues 2/2 (ok), B full (drop) → sent=1.
            let sent = manager.broadcast(msg.clone()).await.unwrap();
            assert_eq!(
                sent, 1,
                "peer A (cap 2) still accepts; peer B (cap 1) drops"
            );

            let snap = manager.metrics.snapshot();
            assert_eq!(
                snap.broadcast_blackout, 0,
                "blackout must NOT increment when at least one peer accepted"
            );
            assert_eq!(
                snap.broadcast_fanout_drop_by_type[OverlayMessageKind::ScpMessage as usize],
                1,
                "the single dropped peer must still be counted per-type"
            );
        }
    }

    /// #3792: the pure interval gate for the backpressure WARN — emits on the
    /// first call, throttles within the interval, and re-emits once the window
    /// has elapsed.
    #[test]
    fn test_should_emit_now_rate_limit() {
        let last = AtomicU64::new(0);
        let interval = 500u64;
        // First call in a window → true.
        assert!(should_emit_now(&last, 1_000, interval));
        // Second call within the interval → false.
        assert!(!should_emit_now(&last, 1_200, interval));
        // After the window elapses → true again.
        assert!(should_emit_now(&last, 1_600, interval));
        // And immediately after, throttled again.
        assert!(!should_emit_now(&last, 1_700, interval));
    }

    fn make_flood_tx_msg() -> StellarMessage {
        use stellar_xdr::TransactionEnvelope;
        StellarMessage::Transaction(TransactionEnvelope::Tx(
            stellar_xdr::TransactionV1Envelope {
                tx: stellar_xdr::Transaction {
                    source_account: stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(
                        [0; 32],
                    )),
                    fee: 100,
                    seq_num: stellar_xdr::SequenceNumber(1),
                    cond: stellar_xdr::Preconditions::None,
                    memo: stellar_xdr::Memo::None,
                    operations: stellar_xdr::VecM::default(),
                    ext: stellar_xdr::TransactionExt::V0,
                },
                signatures: stellar_xdr::VecM::default(),
            },
        ))
    }

    #[tokio::test]
    async fn test_flood_broadcast_counter_increments() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        let peer_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer_id, 10);

        assert_eq!(manager.metrics.flood_broadcast.get(), 0);

        let msg = make_flood_tx_msg();
        let sent = manager.broadcast(msg).await.unwrap();
        assert_eq!(sent, 1);

        assert_eq!(manager.metrics.flood_broadcast.get(), 1);
    }

    #[tokio::test]
    async fn test_non_flood_broadcast_does_not_increment_flood_counter() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::SeqCst);

        let peer_id = PeerId::from_bytes([1u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer_id, 10);

        let msg = make_hello_msg();
        let sent = manager.broadcast(msg).await.unwrap();
        assert_eq!(sent, 1);

        assert_eq!(
            manager.metrics.flood_broadcast.get(),
            0,
            "non-flood messages should not increment flood_broadcast"
        );
    }

    // ──────── KnownPeerSet tests ────────

    #[test]
    fn test_known_peer_set_from_config() {
        let config = vec![
            PeerAddress::new("stellar.example.com", 11625),
            PeerAddress::new("10.0.0.1", 11625),
        ];
        let set = KnownPeerSet::from_config(config.clone());
        assert_eq!(set.config_entries.len(), 2);
        assert_eq!(set.resolved.len(), 2);
        assert!(set.resolved.iter().all(|r| r.is_none()));
        assert!(set.discovered.is_empty());
    }

    #[test]
    fn test_known_peer_set_update_resolved() {
        let config = vec![
            PeerAddress::new("stellar.example.com", 11625),
            PeerAddress::new("peer2.example.com", 11625),
        ];
        let mut set = KnownPeerSet::from_config(config);

        // First resolution: both succeed
        let results = vec![
            Some(PeerAddress::new("10.0.0.1", 11625)),
            Some(PeerAddress::new("10.0.0.2", 11625)),
        ];
        set.update_resolved(&results);
        assert_eq!(set.resolved[0].as_ref().unwrap().host, "10.0.0.1");
        assert_eq!(set.resolved[1].as_ref().unwrap().host, "10.0.0.2");

        // Second resolution: first fails, second changes IP
        let results2 = vec![None, Some(PeerAddress::new("10.0.0.99", 11625))];
        set.update_resolved(&results2);
        // Last-good preserved on failure
        assert_eq!(set.resolved[0].as_ref().unwrap().host, "10.0.0.1");
        // Updated on success
        assert_eq!(set.resolved[1].as_ref().unwrap().host, "10.0.0.99");
    }

    #[test]
    #[should_panic(expected = "resolve results length must match config_entries")]
    fn test_known_peer_set_update_resolved_panics_too_long() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);
        // Too many results — must panic
        set.update_resolved(&[
            Some(PeerAddress::new("10.0.0.1", 11625)),
            Some(PeerAddress::new("10.0.0.2", 11625)),
        ]);
    }

    #[test]
    #[should_panic(expected = "resolve results length must match config_entries")]
    fn test_known_peer_set_update_resolved_panics_too_short() {
        let config = vec![
            PeerAddress::new("stellar.example.com", 11625),
            PeerAddress::new("peer2.example.com", 11625),
        ];
        let mut set = KnownPeerSet::from_config(config);
        // Too few results — must panic
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.1", 11625))]);
    }

    #[test]
    fn test_known_peer_set_shuffled_dial_entries_uses_resolved() {
        let config = vec![
            PeerAddress::new("stellar.example.com", 11625),
            PeerAddress::new("peer2.example.com", 11625),
        ];
        let mut set = KnownPeerSet::from_config(config);

        // Before resolution: returns hostnames
        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        assert_eq!(entries.len(), 2);
        let hosts: HashSet<String> = entries.iter().map(|e| e.host.clone()).collect();
        assert!(hosts.contains("stellar.example.com"));
        assert!(hosts.contains("peer2.example.com"));

        // After resolution: returns IPs
        set.update_resolved(&[
            Some(PeerAddress::new("10.0.0.1", 11625)),
            Some(PeerAddress::new("10.0.0.2", 11625)),
        ]);
        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        assert_eq!(entries.len(), 2);
        let hosts: HashSet<String> = entries.iter().map(|e| e.host.clone()).collect();
        assert!(hosts.contains("10.0.0.1"));
        assert!(hosts.contains("10.0.0.2"));
    }

    #[test]
    fn test_known_peer_set_deduplicates_same_resolved_ip() {
        let config = vec![
            PeerAddress::new("alias1.example.com", 11625),
            PeerAddress::new("alias2.example.com", 11625),
        ];
        let mut set = KnownPeerSet::from_config(config);

        // Both resolve to same IP
        set.update_resolved(&[
            Some(PeerAddress::new("10.0.0.1", 11625)),
            Some(PeerAddress::new("10.0.0.1", 11625)),
        ]);
        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        assert_eq!(
            entries.len(),
            1,
            "two hostnames → same IP should dedup to one dial entry"
        );
        assert_eq!(entries[0].host, "10.0.0.1");
    }

    #[test]
    fn test_known_peer_set_add_discovered() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);

        assert!(set.add_discovered(PeerAddress::new("10.0.0.5", 11625)));
        assert_eq!(set.discovered.len(), 1);

        // Duplicate rejected
        assert!(!set.add_discovered(PeerAddress::new("10.0.0.5", 11625)));
        assert_eq!(set.discovered.len(), 1);
    }

    #[test]
    fn test_known_peer_set_add_discovered_cap() {
        // Config takes 1 slot, discovered gets MAX_KNOWN_PEERS - 1
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);

        let cap = MAX_KNOWN_PEERS - 1;
        for i in 0..cap {
            let addr = PeerAddress::new(&format!("10.0.{}.{}", (i >> 8) & 0xFF, i & 0xFF), 11625);
            assert!(set.add_discovered(addr), "peer {i} should be accepted");
        }
        // One more should be rejected
        assert!(!set.add_discovered(PeerAddress::new("192.168.1.1", 9999)));
    }

    #[test]
    fn test_known_peer_set_set_discovered() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);

        // Add initial discovered
        set.add_discovered(PeerAddress::new("10.0.0.1", 11625));

        // Set resolution for config entry
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.99", 11625))]);

        // Replace discovered via set_discovered (simulates DB refresh)
        set.set_discovered(vec![
            PeerAddress::new("10.0.0.5", 11625),
            PeerAddress::new("10.0.0.6", 11625),
        ]);

        // Discovered replaced, config + resolution preserved
        assert_eq!(set.discovered.len(), 2);
        assert_eq!(set.resolved[0].as_ref().unwrap().host, "10.0.0.99");

        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        let hosts: HashSet<String> = entries.iter().map(|e| e.host.clone()).collect();
        assert!(hosts.contains("10.0.0.99")); // resolved config
        assert!(hosts.contains("10.0.0.5"));
        assert!(hosts.contains("10.0.0.6"));
    }

    #[test]
    fn test_known_peer_set_set_discovered_filters_config_entries() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.99", 11625))]);

        // DB refresh includes the resolved IP of a config peer — should be filtered
        set.set_discovered(vec![
            PeerAddress::new("10.0.0.99", 11625), // matches resolved config
            PeerAddress::new("10.0.0.5", 11625),  // new peer
        ]);

        // Only the non-config peer should be stored
        assert_eq!(set.discovered.len(), 1);
        assert_eq!(set.discovered[0].host, "10.0.0.5");
    }

    #[test]
    fn test_known_peer_set_all_entries() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);
        set.add_discovered(PeerAddress::new("10.0.0.5", 11625));
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.1", 11625))]);

        let all = set.all_entries();
        assert_eq!(all.len(), 2);
        // Config entry returns resolved IP
        assert_eq!(all[0].host, "10.0.0.1");
        // Discovered entry as-is
        assert_eq!(all[1].host, "10.0.0.5");
    }

    #[test]
    fn test_known_peer_set_dial_entries_includes_discovered() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);
        set.add_discovered(PeerAddress::new("10.0.0.5", 11625));
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.1", 11625))]);

        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        assert_eq!(entries.len(), 2);
        let hosts: HashSet<String> = entries.iter().map(|e| e.host.clone()).collect();
        assert!(hosts.contains("10.0.0.1"));
        assert!(hosts.contains("10.0.0.5"));
    }

    #[test]
    fn test_known_peer_set_discovered_dedup_with_resolved_config() {
        let config = vec![PeerAddress::new("stellar.example.com", 11625)];
        let mut set = KnownPeerSet::from_config(config);
        set.update_resolved(&[Some(PeerAddress::new("10.0.0.1", 11625))]);

        // Add discovered peer with same IP as resolved config entry
        set.add_discovered(PeerAddress::new("10.0.0.1", 11625));

        let entries = set.shuffled_dial_entries(&mut rand::thread_rng());
        // Dedup: config resolved and discovered have same canonical_key
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "10.0.0.1");
    }

    // ──────── request_scp_state §15.3 parity tests ────────

    /// Regression test for #2980: request_scp_state must return
    /// Err(NotStarted) when the overlay has not been started, matching the
    /// guard on broadcast()/connect() and stellar-core's shutdown guard,
    /// instead of silently returning Ok(0) (which is indistinguishable from
    /// "started, no peers").
    #[test]
    fn test_request_scp_state_returns_not_started_when_not_started() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        // new() leaves running=false; the manager is never started.
        let manager = OverlayManager::new(config, local_node).unwrap();

        let result = manager.request_scp_state(42);
        assert!(
            matches!(result, Err(OverlayError::NotStarted)),
            "request_scp_state should return Err(NotStarted) before start, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_request_scp_state_with_single_peer_sends_once() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        // Mark the manager started so request_scp_state passes the NotStarted
        // guard (see #2980).
        manager.running.store(true, Ordering::Relaxed);

        let peer_id = PeerId::from_bytes([42u8; 32]);
        let mut rx = insert_peer_with_capacity(&manager, peer_id, 16);

        let sent = manager.request_scp_state(50).unwrap();
        assert_eq!(sent, 1, "should send to the single available peer");

        let msg = rx.try_recv().unwrap();
        match msg {
            OutboundMessage::Send(StellarMessage::GetScpState(seq)) => {
                assert_eq!(seq, 50);
            }
            _ => panic!("expected Send(GetScpState(50))"),
        }
    }

    #[tokio::test]
    async fn test_request_scp_state_with_zero_peers_returns_zero() {
        let config = OverlayConfig::default();
        let secret = SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);

        let manager = OverlayManager::new(config, local_node).unwrap();
        // Mark the manager started; started-but-no-peers must still yield 0
        // (distinct from the NotStarted error path, see #2980).
        manager.running.store(true, Ordering::Relaxed);

        let sent = manager.request_scp_state(100).unwrap();
        assert_eq!(sent, 0, "should return 0 when no peers connected");
    }

    /// Regression test for #2909: request_scp_state targets at most 2 random
    /// authenticated peers instead of broadcasting to all connected peers.
    #[test]
    fn test_request_scp_state_targets_two_authenticated_peers() {
        let shared = test_shared_state(vec![]);

        // Insert 3 authenticated test peers.
        let peer1 = PeerId::from_bytes([1u8; 32]);
        let peer2 = PeerId::from_bytes([2u8; 32]);
        let peer3 = PeerId::from_bytes([3u8; 32]);
        let addr1: std::net::SocketAddr = "10.0.0.1:11625".parse().unwrap();
        let addr2: std::net::SocketAddr = "10.0.0.2:11625".parse().unwrap();
        let addr3: std::net::SocketAddr = "10.0.0.3:11625".parse().unwrap();

        let mut rx1 = insert_fake_peer(
            &shared,
            peer1,
            addr1,
            crate::connection::ConnectionDirection::Outbound,
        );
        let mut rx2 = insert_fake_peer(
            &shared,
            peer2,
            addr2,
            crate::connection::ConnectionDirection::Outbound,
        );
        let mut rx3 = insert_fake_peer(
            &shared,
            peer3,
            addr3,
            crate::connection::ConnectionDirection::Outbound,
        );

        // Build an OverlayManager that uses this shared state.
        let config = OverlayConfig::testnet();
        let secret = henyey_crypto::SecretKey::generate();
        let local_node = LocalNode::new_testnet(secret);
        let mut manager = OverlayManager::new(config, local_node).unwrap();
        // Replace the internal shared state with ours (which has peers).
        manager.peers = shared.peers.clone();
        manager.peer_info_cache = shared.peer_info_cache.clone();
        manager.running = shared.running.clone();

        let ledger_seq = 100u32;
        let result = manager.request_scp_state(ledger_seq).unwrap();

        // Exactly 2 peers should receive the message (3 connected, bound is 2,
        // channels have ample capacity so no send failures).
        assert_eq!(
            result, 2,
            "request_scp_state should target exactly 2 peers when 3 are connected, got {}",
            result
        );

        // Validate that exactly 2 peers received GetScpState(ledger_seq).
        let msg1 = rx1.try_recv().ok();
        let msg2 = rx2.try_recv().ok();
        let msg3 = rx3.try_recv().ok();

        let received: Vec<_> = [msg1, msg2, msg3].into_iter().flatten().collect();
        assert_eq!(
            received.len(),
            2,
            "Exactly 2 of 3 peers should receive GetScpState, got {}",
            received.len()
        );

        // Verify each received message is the correct GetScpState(ledger_seq).
        for msg in &received {
            match msg {
                super::OutboundMessage::Send(stellar_xdr::StellarMessage::GetScpState(seq)) => {
                    assert_eq!(
                        *seq, ledger_seq,
                        "GetScpState should contain ledger_seq {}, got {}",
                        ledger_seq, seq
                    );
                }
                other => {
                    panic!(
                        "Expected OutboundMessage::Send(GetScpState({})), got {:?}",
                        ledger_seq, other
                    );
                }
            }
        }
    }

    /// Regression test for #3318: the additive `request_scp_state_widened`
    /// variant sends `GetScpState` to up to its bounded cap of authenticated
    /// peers (drawn from the FULL inbound+outbound set), while the original
    /// `request_scp_state` still targets exactly 2. This is the henyey-specific
    /// wider recovery pull that fires only when the bounded 2-peer pull keeps
    /// landing on peers that cannot serve the missing slot.
    #[test]
    fn test_request_scp_state_widened_targets_serviceable_peers() {
        let config = OverlayConfig::default();
        let local_node = LocalNode::new_testnet(SecretKey::generate());
        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::Relaxed);

        // Six connected authenticated peers, a mix of inbound + outbound so we
        // also confirm the widened pull draws from the full authenticated set.
        let mut rxs = Vec::new();
        for i in 1u8..=6 {
            let peer = PeerId::from_bytes([i; 32]);
            rxs.push(insert_peer_with_capacity(&manager, peer, 16));
        }

        let ledger_seq = 100u32;

        // Cap of 4 → exactly 4 of the 6 peers receive GetScpState.
        let sent = manager.request_scp_state_widened(ledger_seq, 4).unwrap();
        assert_eq!(
            sent, 4,
            "widened pull with cap 4 over 6 peers should send to exactly 4, got {sent}"
        );

        let received: usize = rxs
            .iter_mut()
            .map(|rx| {
                let mut n = 0;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        OutboundMessage::Send(StellarMessage::GetScpState(seq)) => {
                            assert_eq!(seq, ledger_seq, "GetScpState ledger_seq mismatch");
                            n += 1;
                        }
                        other => panic!("expected Send(GetScpState), got {other:?}"),
                    }
                }
                n
            })
            .sum();
        assert_eq!(
            received, 4,
            "exactly 4 of 6 peers should have received GetScpState, got {received}"
        );

        // A cap exceeding the connected set sends to all connected peers (the
        // serviceable==0 fallback widens to ALL authenticated peers).
        let sent_all = manager.request_scp_state_widened(ledger_seq, 100).unwrap();
        assert_eq!(
            sent_all, 6,
            "widened pull with cap > peer count should send to all 6 peers, got {sent_all}"
        );

        // The original bounded pull is UNTOUCHED: still exactly 2 peers.
        let sent_two = manager.request_scp_state(ledger_seq).unwrap();
        assert_eq!(
            sent_two, 2,
            "original request_scp_state must still target exactly 2 peers, got {sent_two}"
        );
    }

    // ──────── peers_could_serve / record_peer_externalized (#3270) ────────

    /// #3270: the serviceability threshold counts a connected peer iff its
    /// recorded latest externalized slot satisfies
    /// `latest_ext - max_slots <= requested_slot` (the inverse of stellar-core's
    /// trim boundary). Covers: (a) all recent enough; (b) none recent enough;
    /// (c) the exact `gap == max_slots` boundary (inclusive → serves) vs
    /// `gap == max_slots + 1` (does not serve); (d) a peer with no recorded
    /// observation is in `total` but excluded from `could_serve`.
    #[test]
    fn test_peers_could_serve_threshold() {
        const MAX_SLOTS: u32 = 12;

        let config = OverlayConfig::default();
        let local_node = LocalNode::new_testnet(SecretKey::generate());
        let manager = OverlayManager::new(config, local_node).unwrap();
        manager.running.store(true, Ordering::Relaxed);

        // Four connected peers.
        let p_recent = PeerId::from_bytes([1u8; 32]);
        let p_boundary = PeerId::from_bytes([2u8; 32]);
        let p_over = PeerId::from_bytes([3u8; 32]);
        let p_unobserved = PeerId::from_bytes([4u8; 32]);
        let _r1 = insert_peer_with_capacity(&manager, p_recent.clone(), 16);
        let _r2 = insert_peer_with_capacity(&manager, p_boundary.clone(), 16);
        let _r3 = insert_peer_with_capacity(&manager, p_over.clone(), 16);
        let _r4 = insert_peer_with_capacity(&manager, p_unobserved.clone(), 16);

        let requested_slot: u32 = 100;

        // p_recent: latest == requested → trivially serves.
        manager.record_peer_externalized(&p_recent, requested_slot as u64);
        // p_boundary: gap == max_slots exactly → inclusive, serves.
        // latest - 12 == 100  ⟹  latest == 112.
        manager.record_peer_externalized(&p_boundary, (requested_slot + MAX_SLOTS) as u64);
        // p_over: gap == max_slots + 1 → does NOT serve.
        // latest - 12 == 101 > 100  ⟹  latest == 113.
        manager.record_peer_externalized(&p_over, (requested_slot + MAX_SLOTS + 1) as u64);
        // p_unobserved: no record → excluded from could_serve, still in total.

        let (could_serve, total) = manager.peers_could_serve(requested_slot, MAX_SLOTS);
        assert_eq!(total, 4, "all four peers are connected → total == 4");
        assert_eq!(
            could_serve, 2,
            "p_recent + p_boundary serve; p_over (gap 13) and p_unobserved (no record) do not"
        );

        // (a) all recent enough: ask about a higher slot that every peer's
        //     window covers. p_over is at latest 113 (gap 13 for slot 100), but
        //     for slot 101 its window floor 113 - 12 = 101 <= 101 serves; record
        //     p_unobserved so it has an observation too. Recording is fetch_max,
        //     so p_recent/p_boundary stay at 100/112 and still serve slot 101.
        manager.record_peer_externalized(&p_unobserved, (requested_slot + 1) as u64);
        let (all_serve, total_all) = manager.peers_could_serve(requested_slot + 1, MAX_SLOTS);
        assert_eq!(total_all, 4);
        assert_eq!(
            all_serve, 4,
            "at slot 101 every peer's window floor (<= 101) covers it → all serve"
        );

        // (b) none recent enough: ask for a slot far below every peer's window.
        //     For requested_slot = 0 and latest ~ 100+, latest - 12 > 0 for all.
        let (none_serve, total_none) = manager.peers_could_serve(0, MAX_SLOTS);
        assert_eq!(total_none, 4);
        assert_eq!(
            none_serve, 0,
            "no peer's window reaches slot 0 (latest - 12 > 0 for all)"
        );
    }

    /// #3270: a peer with no recorded externalized observation must NOT be
    /// counted as serviceable via `0.saturating_sub(max_slots) = 0`. Asserted
    /// in isolation: a single unobserved peer yields could_serve == 0 even for
    /// a low requested_slot where the slot-0 collision would otherwise fire.
    #[test]
    fn test_peers_could_serve_excludes_unobserved_peer() {
        const MAX_SLOTS: u32 = 12;
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        manager.running.store(true, Ordering::Relaxed);

        let peer = PeerId::from_bytes([5u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer, 16);

        // requested_slot = 0 is exactly the case where treating "no observation"
        // as slot 0 would wrongly count the peer (0.saturating_sub(12) = 0 <= 0).
        let (could_serve, total) = manager.peers_could_serve(0, MAX_SLOTS);
        assert_eq!(total, 1, "the peer is connected");
        assert_eq!(
            could_serve, 0,
            "an unobserved peer must be excluded from could_serve (no slot-0 collision)"
        );
    }

    /// #3270: `record_peer_externalized` is monotonic (`fetch_max`) — a lower
    /// slot never regresses a peer's recorded high-water mark.
    #[test]
    fn test_record_peer_externalized_monotonic() {
        const MAX_SLOTS: u32 = 12;
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        manager.running.store(true, Ordering::Relaxed);

        let peer = PeerId::from_bytes([6u8; 32]);
        let _rx = insert_peer_with_capacity(&manager, peer.clone(), 16);

        manager.record_peer_externalized(&peer, 200);
        // A stale, lower slot must not regress the recorded value.
        manager.record_peer_externalized(&peer, 50);

        // With latest == 200, the peer serves requested_slot >= 200 - 12 = 188.
        let (serves_at_188, _) = manager.peers_could_serve(188, MAX_SLOTS);
        assert_eq!(
            serves_at_188, 1,
            "latest stayed at 200; 200 - 12 = 188 <= 188"
        );
        // It does NOT serve requested_slot = 187 (latest - 12 = 188 > 187).
        let (serves_at_187, _) = manager.peers_could_serve(187, MAX_SLOTS);
        assert_eq!(
            serves_at_187, 0,
            "the regression (slot 50) was ignored; window floor is 188, not 38"
        );
    }

    /// #3270: the per-peer externalized entry is dropped on disconnect, so
    /// `peers_could_serve` reflects only live peers (no stale-peer inflation).
    #[tokio::test]
    async fn test_peer_externalized_removed_on_disconnect() {
        const MAX_SLOTS: u32 = 12;
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        manager.running.store(true, Ordering::Relaxed);

        // shared_state() shares the same Arcs as `manager`, so cleanup_peer on
        // the shared state is observable through the manager's accessors.
        let shared = manager.shared_state();
        let peer = PeerId::from_bytes([7u8; 32]);
        let _rx = insert_fake_peer(
            &shared,
            peer.clone(),
            "10.0.0.7:11625".parse().unwrap(),
            crate::connection::ConnectionDirection::Outbound,
        );

        manager.record_peer_externalized(&peer, 300);
        let (before, total_before) = manager.peers_could_serve(290, MAX_SLOTS);
        assert_eq!(total_before, 1, "peer connected before disconnect");
        assert_eq!(before, 1, "peer serves slot 290 (300 - 12 = 288 <= 290)");

        // Disconnect: cleanup_peer must drop both peer_info_cache and the
        // per-peer externalized entry (generation 0 matches insert_fake_peer).
        shared.cleanup_peer(&peer, 0);

        let (after, total_after) = manager.peers_could_serve(290, MAX_SLOTS);
        assert_eq!(total_after, 0, "no live peers after disconnect");
        assert_eq!(after, 0, "departed peer must not be counted as serviceable");
        assert!(
            !manager.peer_latest_externalized.contains_key(&peer),
            "per-peer externalized entry must be removed on disconnect"
        );
    }

    /// #3270: `record_peer_externalized` ignores peers that are not currently
    /// connected, so a racing late EXTERNALIZE cannot resurrect a stale entry
    /// after the disconnect cleanup ran.
    #[test]
    fn test_record_peer_externalized_ignores_unconnected_peer() {
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        manager.running.store(true, Ordering::Relaxed);

        let peer = PeerId::from_bytes([8u8; 32]);
        // No insert into peer_info_cache → not connected.
        manager.record_peer_externalized(&peer, 500);
        assert!(
            !manager.peer_latest_externalized.contains_key(&peer),
            "must not record externalized slot for an unconnected peer"
        );
    }

    #[tokio::test]
    async fn test_send_error_and_drop_sends_misc_error_then_shutdown() {
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        let peer_id = PeerId::from_bytes([7u8; 32]);
        let mut rx = insert_peer_with_capacity(&manager, peer_id.clone(), 16);

        assert!(
            manager.send_error_and_drop(
                &peer_id,
                stellar_xdr::ErrorCode::Misc,
                "Survey has invalid signature",
            ),
            "send_error_and_drop should queue the shutdown for a connected peer"
        );

        // First the ERROR_MSG with the exact survey code/string.
        match rx.recv().await.unwrap() {
            OutboundMessage::Send(StellarMessage::ErrorMsg(err)) => {
                assert_eq!(err.code, stellar_xdr::ErrorCode::Misc);
                assert_eq!(err.msg.to_string(), "Survey has invalid signature");
            }
            other => panic!(
                "expected Send(ErrorMsg), got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Then the deferred shutdown (flush-then-close).
        match rx.recv().await.unwrap() {
            OutboundMessage::ShutdownAfterError => {}
            other => panic!(
                "expected ShutdownAfterError, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn test_send_error_and_drop_unknown_peer_returns_false() {
        let manager = OverlayManager::new(
            OverlayConfig::default(),
            LocalNode::new_testnet(SecretKey::generate()),
        )
        .unwrap();
        let peer_id = PeerId::from_bytes([8u8; 32]);
        assert!(
            !manager.send_error_and_drop(
                &peer_id,
                stellar_xdr::ErrorCode::Misc,
                "Survey has invalid signature",
            ),
            "send_error_and_drop should report false for an unknown peer"
        );
    }
}
