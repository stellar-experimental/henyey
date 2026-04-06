//! Flow control for Stellar overlay connections.
//!
//! This module implements flow control as specified in the stellar-core
//! `FlowControl.h` and `FlowControlCapacity.h`. It manages message
//! throttling between peers to prevent overwhelming connections.
//!
//! # Overview
//!
//! Flow control tracks two types of capacity:
//! - **Message capacity**: Number of messages that can be sent/received
//! - **Byte capacity**: Total bytes that can be sent/received
//!
//! Messages are prioritized in queues:
//! 1. SCP messages (highest priority - critical for consensus)
//! 2. Transactions
//! 3. Flood demands
//! 4. Flood adverts (lowest priority)
//!
//! # Protocol
//!
//! 1. After authentication, peers exchange `SEND_MORE_EXTENDED` messages
//! 2. Each `SEND_MORE_EXTENDED` grants capacity (messages and bytes)
//! 3. When capacity is exhausted, the sender must wait for more capacity
//! 4. The receiver sends `SEND_MORE_EXTENDED` after processing messages

use crate::{OverlayError, PeerId, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use stellar_xdr::curr::{StellarMessage, WriteXdr};
use tracing::{debug, trace, warn};

/// Callback trait for SCP queue trimming decisions.
///
/// The overlay layer needs information from the herder to make intelligent
/// trimming decisions for the SCP message queue. This trait abstracts that
/// dependency so the overlay crate doesn't depend on the herder directly.
pub trait ScpQueueCallback: Send + Sync {
    /// Returns the minimum slot index to keep in memory.
    /// Messages for slots below this value (except checkpoint seq) are dropped.
    fn min_slot_to_remember(&self) -> u64;

    /// Returns the most recent checkpoint sequence number.
    /// Messages at this slot index are preserved even if below min_slot_to_remember.
    fn most_recent_checkpoint_seq(&self) -> u64;
}

/// Configuration for flow control.
///
/// Default values match stellar-core Config.cpp defaults:
/// - `PEER_FLOOD_READING_CAPACITY = 200`
/// - `PEER_READING_CAPACITY = 201`
/// - `FLOW_CONTROL_SEND_MORE_BATCH_SIZE = 40`
/// - `FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES = 100_000` (initial default)
#[derive(Debug, Clone)]
pub struct FlowControlConfig {
    /// Maximum flood messages we can process before sending SEND_MORE.
    ///
    /// stellar-core: `PEER_FLOOD_READING_CAPACITY` (default 200).
    pub peer_flood_reading_capacity: u64,
    /// Maximum total messages we can process before sending SEND_MORE.
    ///
    /// stellar-core: `PEER_READING_CAPACITY` (default 201).
    pub peer_reading_capacity: u64,
    /// Batch size for flood messages before requesting more.
    ///
    /// stellar-core: `FLOW_CONTROL_SEND_MORE_BATCH_SIZE` (default 40).
    pub flow_control_send_more_batch_size: u64,
    /// Maximum bytes in outbound transaction queue (3 MB).
    pub outbound_tx_queue_byte_limit: usize,
    /// Maximum operations in a transaction set (used for queue limits).
    pub max_tx_set_size_ops: u32,
    /// Byte batch size for flood messages.
    ///
    /// stellar-core: `INITIAL_FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES` (default 100 000).
    pub flow_control_bytes_batch_size: u64,
}

impl Default for FlowControlConfig {
    fn default() -> Self {
        Self {
            // Match stellar-core defaults from Config.cpp
            peer_flood_reading_capacity: 200,
            peer_reading_capacity: 201,
            flow_control_send_more_batch_size: 40,
            outbound_tx_queue_byte_limit: 3 * 1024 * 1024, // 3 MB (match stellar-core)
            max_tx_set_size_ops: 10000,
            flow_control_bytes_batch_size: 100_000, // 100 KB (spec: OVERLAY_SPEC §5.1)
        }
    }
}

/// Capacity to send in a SEND_MORE_EXTENDED message.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendMoreCapacity {
    /// Number of flood messages to request.
    pub num_flood_messages: u64,
    /// Number of flood bytes to request.
    pub num_flood_bytes: u64,
    /// Total messages processed (for non-flood messages).
    pub num_total_messages: u32,
}

impl SendMoreCapacity {
    /// Returns true if we should send a SEND_MORE message.
    pub fn should_send(&self) -> bool {
        self.num_flood_messages > 0 || self.num_flood_bytes > 0
    }
}

/// Message priority levels for outbound queuing.
/// Lower values = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum MessagePriority {
    /// SCP messages - highest priority, critical for consensus.
    Scp = 0,
    /// Transaction messages.
    Transaction = 1,
    /// Flood demand messages.
    FloodDemand = 2,
    /// Flood advert messages - lowest priority.
    FloodAdvert = 3,
}

impl MessagePriority {
    /// Number of priority levels.
    pub const COUNT: usize = 4;

    /// Get the priority for a message type.
    pub fn from_message(msg: &StellarMessage) -> Option<Self> {
        match msg {
            StellarMessage::ScpMessage(_) => Some(Self::Scp),
            StellarMessage::Transaction(_) => Some(Self::Transaction),
            StellarMessage::FloodDemand(_) => Some(Self::FloodDemand),
            StellarMessage::FloodAdvert(_) => Some(Self::FloodAdvert),
            _ => None,
        }
    }
}

/// A message queued for outbound sending.
#[derive(Debug, Clone)]
pub struct QueuedOutboundMessage {
    /// The message to send.
    pub message: StellarMessage,
    /// When the message was added to the queue.
    pub time_emplaced: Instant,
    /// Whether this message is currently being sent.
    pub being_sent: bool,
}

/// Reading capacity for local node.
#[derive(Debug, Clone, Copy)]
struct ReadingCapacity {
    /// Capacity for flood messages.
    flood_capacity: u64,
    /// Capacity for all messages (if Some).
    total_capacity: Option<u64>,
}

/// Flow control capacity tracking, parameterized by resource-counting strategy.
///
/// Used for both message-based capacity (where each message costs 1 unit) and
/// byte-based capacity (where each message costs its serialized size in bytes).
/// The `resource_counter` function pointer determines which unit is used.
struct FlowControlCapacity {
    /// Current local reading capacity.
    capacity: ReadingCapacity,
    /// Capacity limits (only used by byte-based tracking for tx size increases).
    capacity_limits: Option<ReadingCapacity>,
    /// Outbound capacity (what the peer allows us to send).
    outbound_capacity: u64,
    /// Returns the resource cost of a message (1 for messages, byte size for bytes).
    resource_counter: fn(&StellarMessage) -> u64,
}

impl FlowControlCapacity {
    /// Create a message-based capacity tracker.
    fn new_message(config: &FlowControlConfig) -> Self {
        Self {
            capacity: ReadingCapacity {
                flood_capacity: config.peer_flood_reading_capacity,
                total_capacity: Some(config.peer_reading_capacity),
            },
            capacity_limits: None,
            outbound_capacity: 0,
            resource_counter: |_| 1,
        }
    }

    /// Create a byte-based capacity tracker.
    fn new_bytes(initial_capacity: u64) -> Self {
        let capacity_limits = ReadingCapacity {
            flood_capacity: initial_capacity,
            total_capacity: None,
        };
        Self {
            capacity: capacity_limits,
            capacity_limits: Some(capacity_limits),
            outbound_capacity: 0,
            resource_counter: msg_body_size,
        }
    }

    fn get_msg_resource_count(&self, msg: &StellarMessage) -> u64 {
        (self.resource_counter)(msg)
    }

    fn has_outbound_capacity(&self, msg: &StellarMessage) -> bool {
        self.outbound_capacity >= self.get_msg_resource_count(msg)
    }

    fn lock_outbound_capacity(&mut self, msg: &StellarMessage) {
        if is_flow_controlled_message(msg) {
            let count = self.get_msg_resource_count(msg);
            debug_assert!(self.outbound_capacity >= count);
            self.outbound_capacity = self.outbound_capacity.saturating_sub(count);
        }
    }

    /// Check if local capacity is sufficient for this message (read-only).
    ///
    /// Mirrors stellar-core's `FlowControlCapacity::canLockLocalCapacity`.
    fn can_lock_local_capacity(&self, msg: &StellarMessage) -> bool {
        let msg_resources = self.get_msg_resource_count(msg);

        if let Some(total) = self.capacity.total_capacity {
            if total < msg_resources {
                return false;
            }
        }

        if is_flow_controlled_message(msg) {
            if self.capacity.flood_capacity < msg_resources {
                return false;
            }
        }

        true
    }

    /// Deduct local capacity for this message. Caller MUST check
    /// `can_lock_local_capacity` first — panics if capacity is insufficient.
    ///
    /// Mirrors stellar-core's `FlowControlCapacity::lockLocalCapacity`.
    fn lock_local_capacity(&mut self, msg: &StellarMessage) {
        assert!(
            self.can_lock_local_capacity(msg),
            "lock_local_capacity called without sufficient capacity"
        );

        let msg_resources = self.get_msg_resource_count(msg);

        if let Some(ref mut total) = self.capacity.total_capacity {
            *total -= msg_resources;
        }

        if is_flow_controlled_message(msg) {
            self.capacity.flood_capacity -= msg_resources;
        }
    }

    fn release_local_capacity(&mut self, msg: &StellarMessage) -> u64 {
        let resources_freed = self.get_msg_resource_count(msg);
        let mut released_flood_capacity = 0;

        if let Some(ref mut total) = self.capacity.total_capacity {
            *total += resources_freed;
        }

        if is_flow_controlled_message(msg) {
            released_flood_capacity = resources_freed;
            self.capacity.flood_capacity += resources_freed;
        }

        released_flood_capacity
    }

    fn release_outbound_capacity(&mut self, amount: u32) {
        self.outbound_capacity += amount as u64;
    }

    fn can_read(&self) -> bool {
        self.capacity.total_capacity.map(|c| c > 0).unwrap_or(true)
    }

    fn get_outbound_capacity(&self) -> u64 {
        self.outbound_capacity
    }

    fn handle_tx_size_increase(&mut self, increase: u32) {
        if let Some(ref mut limits) = self.capacity_limits {
            self.capacity.flood_capacity += increase as u64;
            limits.flood_capacity += increase as u64;
        }
    }
}

/// Internal state protected by mutex.
struct FlowControlState {
    /// Message capacity tracker.
    message_capacity: FlowControlCapacity,
    /// Byte capacity tracker.
    byte_capacity: FlowControlCapacity,
    /// Outbound queues by priority.
    outbound_queues: [VecDeque<QueuedOutboundMessage>; MessagePriority::COUNT],
    /// Transaction hash count in advert queue.
    advert_queue_tx_hash_count: usize,
    /// Transaction hash count in demand queue.
    demand_queue_tx_hash_count: usize,
    /// Byte count in transaction queue.
    tx_queue_byte_count: usize,
    /// Flood messages processed since last SEND_MORE.
    flood_data_processed: u64,
    /// Flood bytes processed since last SEND_MORE.
    flood_data_processed_bytes: u64,
    /// Total messages processed (for throttling).
    total_msgs_processed: u64,
    /// Time when we last had no outbound capacity.
    no_outbound_capacity: Option<Instant>,
    /// Time when throttling started.
    last_throttle: Option<Instant>,
    /// Peer ID for logging.
    peer_id: Option<PeerId>,
}

/// Flow control manager for a peer connection.
///
/// Thread-safe flow control implementation that tracks capacity for
/// both messages and bytes, manages outbound message queues with
/// priority, and performs load shedding when queues are full.
pub struct FlowControl {
    /// Configuration.
    config: FlowControlConfig,
    /// Protected state.
    state: Mutex<FlowControlState>,
    /// Optional callback for intelligent SCP queue trimming.
    scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    /// Metrics - messages dropped from SCP queue.
    pub(crate) dropped_scp: AtomicU64,
    /// Metrics - messages dropped from TX queue.
    pub(crate) dropped_txs: AtomicU64,
    /// Metrics - messages dropped from advert queue.
    pub(crate) dropped_adverts: AtomicU64,
    /// Metrics - messages dropped from demand queue.
    pub(crate) dropped_demands: AtomicU64,
}

impl FlowControl {
    /// Create a new flow control instance.
    pub fn new(config: FlowControlConfig) -> Self {
        Self::with_scp_callback(config, None)
    }

    /// Create a new flow control instance with an SCP queue callback.
    pub fn with_scp_callback(
        config: FlowControlConfig,
        scp_callback: Option<Arc<dyn ScpQueueCallback>>,
    ) -> Self {
        // Spec: OVERLAY_SPEC §5.2 — initial byte capacity is
        // FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES (the byte batch size itself),
        // NOT peer_flood_reading_capacity * byte_batch_size.
        // stellar-core: FlowControlCapacity.cpp initializes mCapacity.mTotalCapacity
        // as getConfig().FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES.
        let initial_bytes_capacity = config.flow_control_bytes_batch_size;

        Self {
            state: Mutex::new(FlowControlState {
                message_capacity: FlowControlCapacity::new_message(&config),
                byte_capacity: FlowControlCapacity::new_bytes(initial_bytes_capacity),
                outbound_queues: Default::default(),
                advert_queue_tx_hash_count: 0,
                demand_queue_tx_hash_count: 0,
                tx_queue_byte_count: 0,
                flood_data_processed: 0,
                flood_data_processed_bytes: 0,
                total_msgs_processed: 0,
                no_outbound_capacity: Some(Instant::now()),
                last_throttle: None,
                peer_id: None,
            }),
            config,
            scp_callback,
            dropped_scp: AtomicU64::new(0),
            dropped_txs: AtomicU64::new(0),
            dropped_adverts: AtomicU64::new(0),
            dropped_demands: AtomicU64::new(0),
        }
    }

    /// Set the peer ID for logging.
    pub fn set_peer_id(&self, peer_id: PeerId) {
        let mut state = self.state.lock().unwrap();
        state.peer_id = Some(peer_id);
    }

    /// Check if we have capacity to send a message to this peer.
    fn has_outbound_capacity(state: &FlowControlState, msg: &StellarMessage) -> bool {
        state.message_capacity.has_outbound_capacity(msg)
            && state.byte_capacity.has_outbound_capacity(msg)
    }

    fn peer_label(state: &FlowControlState) -> String {
        state
            .peer_id
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn enqueue_message_resources(
        state: &mut FlowControlState,
        msg: &StellarMessage,
        tx_queue_byte_limit: usize,
    ) -> bool {
        match msg {
            StellarMessage::Transaction(_) => {
                let bytes = state.byte_capacity.get_msg_resource_count(msg) as usize;
                if bytes > tx_queue_byte_limit {
                    return false;
                }
                state.tx_queue_byte_count += bytes;
            }
            StellarMessage::FloodDemand(demand) => {
                state.demand_queue_tx_hash_count += demand.tx_hashes.len();
            }
            StellarMessage::FloodAdvert(advert) => {
                state.advert_queue_tx_hash_count += advert.tx_hashes.len();
            }
            _ => {}
        }

        true
    }

    fn dequeue_message_resources(state: &mut FlowControlState, msg: &StellarMessage) {
        match msg {
            StellarMessage::Transaction(_) => {
                let bytes = state.byte_capacity.get_msg_resource_count(msg) as usize;
                state.tx_queue_byte_count = state.tx_queue_byte_count.saturating_sub(bytes);
            }
            StellarMessage::FloodDemand(demand) => {
                state.demand_queue_tx_hash_count = state
                    .demand_queue_tx_hash_count
                    .saturating_sub(demand.tx_hashes.len());
            }
            StellarMessage::FloodAdvert(advert) => {
                state.advert_queue_tx_hash_count = state
                    .advert_queue_tx_hash_count
                    .saturating_sub(advert.tx_hashes.len());
            }
            _ => {}
        }
    }

    /// Release outbound capacity when receiving SEND_MORE_EXTENDED.
    pub fn maybe_release_capacity(&self, msg: &StellarMessage) {
        if let StellarMessage::SendMoreExtended(send_more) = msg {
            let mut state = self.state.lock().unwrap();

            if state.no_outbound_capacity.is_some() {
                // Record throttle duration
                state.no_outbound_capacity = None;
            }

            state
                .message_capacity
                .release_outbound_capacity(send_more.num_messages);
            state
                .byte_capacity
                .release_outbound_capacity(send_more.num_bytes);

            trace!(
                "Received SEND_MORE_EXTENDED: {} messages, {} bytes",
                send_more.num_messages,
                send_more.num_bytes
            );
        }
    }

    /// Handle transaction size increase due to protocol upgrade.
    pub fn handle_tx_size_increase(&self, increase: u32) {
        if increase > 0 {
            let mut state = self.state.lock().unwrap();
            state.byte_capacity.handle_tx_size_increase(increase);
        }
    }

    /// Add a message to the outbound queue, potentially trimming obsolete messages.
    pub fn add_msg_and_maybe_trim_queue(&self, msg: StellarMessage) {
        let priority = match MessagePriority::from_message(&msg) {
            Some(p) => p,
            None => {
                warn!("Unknown message type for flow control queue");
                return;
            }
        };

        let mut state = self.state.lock().unwrap();
        let queue_idx = priority as usize;

        if !Self::enqueue_message_resources(
            &mut state,
            &msg,
            self.config.outbound_tx_queue_byte_limit,
        ) {
            return;
        }

        // Add to queue
        state.outbound_queues[queue_idx].push_back(QueuedOutboundMessage {
            message: msg,
            time_emplaced: Instant::now(),
            being_sent: false,
        });

        // Trim queue if over limits
        let limit = self.config.max_tx_set_size_ops as usize;
        let mut dropped = 0usize;

        match priority {
            MessagePriority::Transaction => {
                let queue_len = state.outbound_queues[queue_idx].len();
                let is_over_limit = queue_len > limit
                    || state.tx_queue_byte_count > self.config.outbound_tx_queue_byte_limit;

                if is_over_limit {
                    dropped = queue_len;
                    state.tx_queue_byte_count = 0;
                    state.outbound_queues[queue_idx].clear();
                    self.dropped_txs
                        .fetch_add(dropped as u64, Ordering::Relaxed);
                }
            }
            MessagePriority::Scp => {
                let queue = &mut state.outbound_queues[queue_idx];
                if queue.len() > limit {
                    dropped = Self::trim_scp_queue(queue, limit, &self.scp_callback);
                    self.dropped_scp
                        .fetch_add(dropped as u64, Ordering::Relaxed);
                }
            }
            MessagePriority::FloodAdvert => {
                if state.advert_queue_tx_hash_count > limit {
                    dropped = state.advert_queue_tx_hash_count;
                    state.advert_queue_tx_hash_count = 0;
                    state.outbound_queues[queue_idx].clear();
                    self.dropped_adverts
                        .fetch_add(dropped as u64, Ordering::Relaxed);
                }
            }
            MessagePriority::FloodDemand => {
                if state.demand_queue_tx_hash_count > limit {
                    dropped = state.demand_queue_tx_hash_count;
                    state.demand_queue_tx_hash_count = 0;
                    state.outbound_queues[queue_idx].clear();
                    self.dropped_demands
                        .fetch_add(dropped as u64, Ordering::Relaxed);
                }
            }
        }

        if dropped > 0 {
            let peer_str = Self::peer_label(&state);
            trace!(
                "Dropped {} {:?} messages to peer {}",
                dropped,
                priority,
                peer_str
            );
        }
    }

    /// Trim the SCP outbound queue when it exceeds the limit.
    ///
    /// Uses the SCP callback (if available) for intelligent trimming:
    /// - Drops messages for slots below `min_slot_to_remember` (except checkpoint)
    /// - Replaces older nomination/ballot messages with newer ones from the back
    ///
    /// Falls back to naive FIFO trimming when no callback is set.
    /// Returns the number of messages dropped.
    fn trim_scp_queue(
        queue: &mut VecDeque<QueuedOutboundMessage>,
        limit: usize,
        scp_callback: &Option<Arc<dyn ScpQueueCallback>>,
    ) -> usize {
        let mut dropped = 0;

        if let Some(ref cb) = scp_callback {
            let min_slot = cb.min_slot_to_remember();
            let checkpoint_seq = cb.most_recent_checkpoint_seq();
            let mut value_replaced = false;

            let mut i = 0;
            while i < queue.len() {
                if queue[i].being_sent {
                    i += 1;
                    continue;
                }

                // Extract slot index from the SCP envelope
                let slot_index = if let StellarMessage::ScpMessage(ref env) = queue[i].message {
                    env.statement.slot_index
                } else {
                    i += 1;
                    continue;
                };

                // Drop messages for slots we no longer care about
                // (except the checkpoint sequence)
                if slot_index < min_slot && slot_index != checkpoint_seq {
                    queue.remove(i);
                    dropped += 1;
                    continue;
                }

                // Replace older nomination/ballot with newer one from the back
                if !value_replaced
                    && i + 1 < queue.len()
                    && !queue.back().map(|m| m.being_sent).unwrap_or(true)
                {
                    let back_st = queue.back().and_then(|m| {
                        if let StellarMessage::ScpMessage(ref env) = m.message {
                            Some(&env.statement)
                        } else {
                            None
                        }
                    });
                    let curr_st = if let StellarMessage::ScpMessage(ref env) = queue[i].message {
                        Some(&env.statement)
                    } else {
                        None
                    };
                    if let (Some(old_st), Some(new_st)) = (curr_st, back_st) {
                        if henyey_scp::is_newer_nomination_or_ballot_st(old_st, new_st) {
                            value_replaced = true;
                            let back = queue.pop_back().unwrap();
                            queue[i] = back;
                            dropped += 1;
                            i += 1;
                            continue;
                        }
                    }
                }

                i += 1;
            }
        } else {
            // Fallback: naive FIFO trimming when no callback is set
            while queue.len() > limit / 2 {
                match queue.front() {
                    Some(front) if !front.being_sent => {
                        queue.pop_front();
                        dropped += 1;
                    }
                    _ => break,
                }
            }
        }

        dropped
    }

    /// Get the next batch of messages to send.
    ///
    /// Returns messages that we have capacity to send, marking them as being sent.
    /// The caller must call `process_sent_messages` after actually sending them.
    pub fn get_next_batch_to_send(&self) -> Vec<QueuedOutboundMessage> {
        let mut state = self.state.lock().unwrap();
        let mut batch = Vec::new();
        let mut to_mark: Vec<(usize, usize, StellarMessage)> = Vec::new();
        let mut out_of_capacity = false;

        'outer: for queue_idx in 0..state.outbound_queues.len() {
            for msg_idx in 0..state.outbound_queues[queue_idx].len() {
                let queued_msg = &state.outbound_queues[queue_idx][msg_idx];
                if queued_msg.being_sent {
                    continue;
                }

                if !Self::has_outbound_capacity(&state, &queued_msg.message) {
                    out_of_capacity = true;
                    break 'outer;
                }

                to_mark.push((queue_idx, msg_idx, queued_msg.message.clone()));
                batch.push(queued_msg.clone());
            }
        }

        if out_of_capacity {
            state.no_outbound_capacity = Some(Instant::now());
        }

        // Mark messages as being sent and lock capacity
        for (queue_idx, msg_idx, msg) in to_mark {
            state.outbound_queues[queue_idx][msg_idx].being_sent = true;
            state.message_capacity.lock_outbound_capacity(&msg);
            state.byte_capacity.lock_outbound_capacity(&msg);
        }

        trace!("Prepared batch of {} messages to send", batch.len());
        batch
    }

    /// Process messages that were successfully sent.
    ///
    /// Removes sent messages from the front of queues and updates byte counts.
    pub fn process_sent_messages(&self, sent_messages: &[Vec<StellarMessage>]) {
        let mut state = self.state.lock().unwrap();

        for (queue_idx, sent_msgs) in sent_messages.iter().enumerate() {
            for msg in sent_msgs {
                if state.outbound_queues[queue_idx].is_empty() {
                    continue;
                }

                // Check if this message is at the front of the queue
                let matches = state.outbound_queues[queue_idx]
                    .front()
                    .is_some_and(|front| messages_equal(&front.message, msg));

                if !matches {
                    continue;
                }

                Self::dequeue_message_resources(&mut state, msg);

                state.outbound_queues[queue_idx].pop_front();
            }
        }
    }

    /// Validate a SEND_MORE_EXTENDED message.
    pub fn is_send_more_valid(&self, msg: &StellarMessage) -> Result<()> {
        let send_more = match msg {
            StellarMessage::SendMoreExtended(sm) => sm,
            _ => {
                return Err(OverlayError::InvalidMessage(
                    "unexpected message type, expected SEND_MORE_EXTENDED".to_string(),
                ));
            }
        };

        if send_more.num_bytes == 0 {
            return Err(OverlayError::InvalidMessage(
                "SEND_MORE_EXTENDED must have non-zero bytes".to_string(),
            ));
        }

        let state = self.state.lock().unwrap();

        // Check for overflow
        let msg_overflow = (send_more.num_messages as u64)
            > u64::MAX - state.message_capacity.get_outbound_capacity();
        let byte_overflow =
            (send_more.num_bytes as u64) > u64::MAX - state.byte_capacity.get_outbound_capacity();

        if msg_overflow || byte_overflow {
            return Err(OverlayError::InvalidMessage(
                "Peer capacity overflow".to_string(),
            ));
        }

        Ok(())
    }

    /// Begin processing a received message.
    ///
    /// Checks both message and byte capacity before locking either.
    /// Returns false if we don't have capacity to process this message.
    ///
    /// Mirrors stellar-core's `FlowControl::beginMessageProcessing`
    /// (FlowControl.cpp:274-290): check both capacities first, then lock
    /// both unconditionally. This avoids leaking one capacity when the
    /// other check fails.
    pub fn begin_message_processing(&self, msg: &StellarMessage) -> bool {
        let mut state = self.state.lock().unwrap();

        if !state.message_capacity.can_lock_local_capacity(msg)
            || !state.byte_capacity.can_lock_local_capacity(msg)
        {
            return false;
        }

        state.message_capacity.lock_local_capacity(msg);
        state.byte_capacity.lock_local_capacity(msg);
        true
    }

    /// End processing a received message.
    ///
    /// Releases local capacity and returns how much capacity to request from the peer.
    pub fn end_message_processing(&self, msg: &StellarMessage) -> SendMoreCapacity {
        let mut state = self.state.lock().unwrap();

        state.flood_data_processed += state.message_capacity.release_local_capacity(msg);
        state.flood_data_processed_bytes += state.byte_capacity.release_local_capacity(msg);
        state.total_msgs_processed += 1;

        let should_send_more = state.flood_data_processed
            >= self.config.flow_control_send_more_batch_size
            || state.flood_data_processed_bytes >= self.config.flow_control_bytes_batch_size;

        let mut result = SendMoreCapacity::default();

        // Check if we've processed enough total messages
        if state.total_msgs_processed >= self.config.peer_reading_capacity {
            result.num_total_messages = state.total_msgs_processed as u32;
            state.total_msgs_processed = 0;
        }

        if should_send_more {
            result.num_flood_messages = state.flood_data_processed;
            result.num_flood_bytes = state.flood_data_processed_bytes;
            state.flood_data_processed = 0;
            state.flood_data_processed_bytes = 0;
        }

        result
    }

    /// Check if we can read more messages from this peer.
    pub fn can_read(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.message_capacity.can_read() && state.byte_capacity.can_read()
    }

    /// Check if no outbound capacity has timed out.
    pub fn no_outbound_capacity_timeout(&self, timeout_secs: u64) -> bool {
        let state = self.state.lock().unwrap();
        if let Some(no_cap_time) = state.no_outbound_capacity {
            no_cap_time.elapsed().as_secs() >= timeout_secs
        } else {
            false
        }
    }

    /// Check if throttling should be applied.
    pub fn maybe_throttle_read(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.message_capacity.can_read() || !state.byte_capacity.can_read() {
            let peer_str = Self::peer_label(&state);
            debug!("Throttle reading from peer {}", peer_str);
            state.last_throttle = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// Stop throttling and return how long we were throttled.
    pub fn stop_throttling(&self) -> Option<std::time::Duration> {
        let mut state = self.state.lock().unwrap();
        if let Some(throttle_time) = state.last_throttle.take() {
            let peer_str = Self::peer_label(&state);
            let duration = throttle_time.elapsed();
            debug!(
                "Stop throttling reading from peer {}, was throttled for {:?}",
                peer_str, duration
            );
            Some(duration)
        } else {
            None
        }
    }

    /// Check if currently throttled.
    pub fn is_throttled(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.last_throttle.is_some()
    }

    /// Get statistics about this flow control instance.
    pub fn get_stats(&self) -> FlowControlStats {
        let state = self.state.lock().unwrap();
        FlowControlStats {
            local_flood_capacity: state.message_capacity.capacity.flood_capacity,
            local_total_capacity: state.message_capacity.capacity.total_capacity,
            peer_message_capacity: state.message_capacity.outbound_capacity,
            local_flood_bytes_capacity: state.byte_capacity.capacity.flood_capacity,
            peer_bytes_capacity: state.byte_capacity.outbound_capacity,
            scp_queue_size: state.outbound_queues[MessagePriority::Scp as usize].len(),
            tx_queue_size: state.outbound_queues[MessagePriority::Transaction as usize].len(),
            demand_queue_size: state.outbound_queues[MessagePriority::FloodDemand as usize].len(),
            advert_queue_size: state.outbound_queues[MessagePriority::FloodAdvert as usize].len(),
            tx_queue_byte_count: state.tx_queue_byte_count,
            is_throttled: state.last_throttle.is_some(),
        }
    }
}

impl Default for FlowControl {
    fn default() -> Self {
        Self::new(FlowControlConfig::default())
    }
}

/// Statistics about flow control state.
#[derive(Debug, Clone)]
pub struct FlowControlStats {
    /// Local flood message capacity remaining.
    pub local_flood_capacity: u64,
    /// Local total message capacity remaining.
    pub local_total_capacity: Option<u64>,
    /// Peer's granted message capacity.
    pub peer_message_capacity: u64,
    /// Local flood byte capacity remaining.
    pub local_flood_bytes_capacity: u64,
    /// Peer's granted byte capacity.
    pub peer_bytes_capacity: u64,
    /// Number of SCP messages queued.
    pub scp_queue_size: usize,
    /// Number of transaction messages queued.
    pub tx_queue_size: usize,
    /// Number of demand messages queued.
    pub demand_queue_size: usize,
    /// Number of advert messages queued.
    pub advert_queue_size: usize,
    /// Bytes in transaction queue.
    pub tx_queue_byte_count: usize,
    /// Whether reading is throttled.
    pub is_throttled: bool,
}

/// Check if a message requires flow control capacity tracking.
///
/// These are the message types that consume flow control capacity (messages
/// and bytes) and are queued through the priority outbound queue. Note that
/// survey messages are flooded at the network routing layer (see
/// [`codec::helpers::is_flood_message`]) but do NOT consume flow control
/// capacity, which is why they are excluded here.
pub fn is_flow_controlled_message(msg: &StellarMessage) -> bool {
    matches!(
        msg,
        StellarMessage::Transaction(_)
            | StellarMessage::ScpMessage(_)
            | StellarMessage::FloodAdvert(_)
            | StellarMessage::FloodDemand(_)
    )
}

/// Get message body size in bytes without heap allocation.
pub(crate) fn msg_body_size(msg: &StellarMessage) -> u64 {
    henyey_common::xdr_encoded_len(msg) as u64
}

/// RAII guard for flow control capacity tracking.
///
/// Mirrors stellar-core's `CapacityTrackedMessage`: the constructor calls
/// `begin_message_processing` to lock local capacity, and `Drop` calls
/// `end_message_processing` to release it.  This guarantees capacity is
/// always released even on early returns or panics.
///
/// For the normal path, call [`CapacityGuard::finish`] to consume the guard
/// and retrieve the [`SendMoreCapacity`] needed to decide whether to send
/// `SEND_MORE_EXTENDED` back to the peer.
pub(crate) struct CapacityGuard {
    flow_control: Arc<FlowControl>,
    message: Option<StellarMessage>,
}

impl CapacityGuard {
    /// Create a new guard, locking local capacity for `msg`.
    ///
    /// Returns `None` if the flow control rejected the message (no capacity).
    pub(crate) fn new(flow_control: Arc<FlowControl>, msg: StellarMessage) -> Option<Self> {
        if flow_control.begin_message_processing(&msg) {
            Some(Self {
                flow_control,
                message: Some(msg),
            })
        } else {
            None
        }
    }

    /// Consume the guard and return the send-more capacity.
    ///
    /// This is the normal-path exit: it releases capacity and tells the
    /// caller whether to send `SEND_MORE_EXTENDED`.
    pub(crate) fn finish(mut self) -> SendMoreCapacity {
        // Take the message so Drop becomes a no-op.
        let msg = self.message.take().expect("finish called twice");
        self.flow_control.end_message_processing(&msg)
    }
}

impl Drop for CapacityGuard {
    fn drop(&mut self) {
        // If `finish()` was called, `message` is `None` and this is a no-op.
        if let Some(ref msg) = self.message {
            let _ = self.flow_control.end_message_processing(msg);
        }
    }
}

/// Compare two messages for equality (by XDR serialization).
fn messages_equal(a: &StellarMessage, b: &StellarMessage) -> bool {
    // Quick check on type first
    if std::mem::discriminant(a) != std::mem::discriminant(b) {
        return false;
    }
    // For full equality, compare XDR
    let a_xdr = a.to_xdr(stellar_xdr::curr::Limits::none());
    let b_xdr = b.to_xdr(stellar_xdr::curr::Limits::none());
    match (a_xdr, b_xdr) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{Hash, ScpEnvelope, SendMoreExtended, TransactionEnvelope};

    fn make_tx_message() -> StellarMessage {
        // Create a minimal transaction message for testing
        StellarMessage::Transaction(TransactionEnvelope::TxV0(
            stellar_xdr::curr::TransactionV0Envelope {
                tx: stellar_xdr::curr::TransactionV0 {
                    source_account_ed25519: stellar_xdr::curr::Uint256([0u8; 32]),
                    fee: 100,
                    seq_num: stellar_xdr::curr::SequenceNumber(1),
                    time_bounds: None,
                    memo: stellar_xdr::curr::Memo::None,
                    operations: vec![].try_into().unwrap(),
                    ext: stellar_xdr::curr::TransactionV0Ext::V0,
                },
                signatures: vec![].try_into().unwrap(),
            },
        ))
    }

    fn make_scp_message() -> StellarMessage {
        StellarMessage::ScpMessage(ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: stellar_xdr::curr::NodeId(
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                        [0u8; 32],
                    )),
                ),
                slot_index: 1,
                pledges: stellar_xdr::curr::ScpStatementPledges::Nominate(
                    stellar_xdr::curr::ScpNomination {
                        quorum_set_hash: Hash([0u8; 32]),
                        votes: vec![].try_into().unwrap(),
                        accepted: vec![].try_into().unwrap(),
                    },
                ),
            },
            signature: stellar_xdr::curr::Signature::default(),
        })
    }

    // ── OVERLAY_SPEC §5.1: FlowControl default constants ─────────────

    #[test]
    fn test_default_config_byte_batch_size_is_100k() {
        let config = FlowControlConfig::default();
        assert_eq!(
            config.flow_control_bytes_batch_size, 100_000,
            "OVERLAY_SPEC §5.1: byte batch size must be 100,000"
        );
    }

    #[test]
    fn test_default_config_constants() {
        let config = FlowControlConfig::default();
        assert_eq!(config.peer_flood_reading_capacity, 200);
        assert_eq!(config.peer_reading_capacity, 201);
        assert_eq!(config.flow_control_send_more_batch_size, 40);
        assert_eq!(config.outbound_tx_queue_byte_limit, 3 * 1024 * 1024);
    }

    #[test]
    fn test_initial_byte_capacity_equals_batch_size() {
        // OVERLAY_SPEC §5.1: initial byte capacity = flow_control_bytes_batch_size,
        // NOT flood_reading_capacity * batch_size.
        let fc = FlowControl::default();
        let stats = fc.get_stats();
        assert_eq!(
            stats.local_flood_bytes_capacity, 100_000,
            "initial byte capacity must equal flow_control_bytes_batch_size"
        );
    }

    #[test]
    fn test_flow_control_creation() {
        let fc = FlowControl::default();
        let stats = fc.get_stats();

        assert_eq!(stats.local_flood_capacity, 200);
        assert_eq!(stats.local_total_capacity, Some(201));
        assert_eq!(stats.peer_message_capacity, 0);
    }

    #[test]
    fn test_message_priority() {
        let tx = make_tx_message();
        let scp = make_scp_message();

        assert_eq!(
            MessagePriority::from_message(&tx),
            Some(MessagePriority::Transaction)
        );
        assert_eq!(
            MessagePriority::from_message(&scp),
            Some(MessagePriority::Scp)
        );
    }

    #[test]
    fn test_is_flow_controlled_message() {
        let tx = make_tx_message();
        let scp = make_scp_message();
        let hello = StellarMessage::Hello(stellar_xdr::curr::Hello {
            ledger_version: 1,
            overlay_version: 1,
            overlay_min_version: 1,
            network_id: Hash([0u8; 32]),
            version_str: "test".try_into().unwrap(),
            listening_port: 0,
            peer_id: stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::curr::Uint256([0u8; 32]),
            )),
            cert: stellar_xdr::curr::AuthCert {
                pubkey: stellar_xdr::curr::Curve25519Public { key: [0u8; 32] },
                expiration: 0,
                sig: stellar_xdr::curr::Signature::default(),
            },
            nonce: stellar_xdr::curr::Uint256([0u8; 32]),
        });

        assert!(is_flow_controlled_message(&tx));
        assert!(is_flow_controlled_message(&scp));
        assert!(!is_flow_controlled_message(&hello));
    }

    #[test]
    fn test_release_capacity() {
        let fc = FlowControl::default();

        // Initially no outbound capacity
        let stats = fc.get_stats();
        assert_eq!(stats.peer_message_capacity, 0);
        assert_eq!(stats.peer_bytes_capacity, 0);

        // Release capacity via SEND_MORE_EXTENDED
        let send_more = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 100,
            num_bytes: 1_000_000,
        });
        fc.maybe_release_capacity(&send_more);

        let stats = fc.get_stats();
        assert_eq!(stats.peer_message_capacity, 100);
        assert_eq!(stats.peer_bytes_capacity, 1_000_000);
    }

    #[test]
    fn test_no_outbound_capacity_timeout_clears_after_send_more_extended() {
        let fc = FlowControl::default();

        assert!(fc.no_outbound_capacity_timeout(0));

        let send_more = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 1,
            num_bytes: 1,
        });
        fc.maybe_release_capacity(&send_more);

        assert!(!fc.no_outbound_capacity_timeout(0));
    }

    #[test]
    fn test_begin_end_message_processing() {
        let fc = FlowControl::default();
        let tx = make_tx_message();

        // Begin processing
        assert!(fc.begin_message_processing(&tx));

        // End processing
        let capacity = fc.end_message_processing(&tx);
        // Should request more after batch
        // (depends on batch size config)
        assert!(capacity.num_flood_messages <= 1);
    }

    #[test]
    fn test_send_more_validation() {
        let fc = FlowControl::default();

        // Valid message
        let valid = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 100,
            num_bytes: 1000,
        });
        assert!(fc.is_send_more_valid(&valid).is_ok());

        // Invalid - zero bytes
        let invalid = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 100,
            num_bytes: 0,
        });
        assert!(fc.is_send_more_valid(&invalid).is_err());

        // Invalid - wrong message type
        let wrong_type = make_tx_message();
        assert!(fc.is_send_more_valid(&wrong_type).is_err());
    }

    #[test]
    fn test_queue_and_batch() {
        let fc = FlowControl::default();

        // Grant some capacity
        let send_more = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 10,
            num_bytes: 100_000,
        });
        fc.maybe_release_capacity(&send_more);

        // Add messages to queue
        let tx = make_tx_message();
        fc.add_msg_and_maybe_trim_queue(tx.clone());
        fc.add_msg_and_maybe_trim_queue(tx.clone());

        let stats = fc.get_stats();
        assert_eq!(stats.tx_queue_size, 2);

        // Get batch
        let batch = fc.get_next_batch_to_send();
        assert_eq!(batch.len(), 2);

        // Process sent messages
        let sent: Vec<Vec<StellarMessage>> = vec![
            vec![],                       // SCP
            vec![tx.clone(), tx.clone()], // TX
            vec![],                       // Demand
            vec![],                       // Advert
        ];
        fc.process_sent_messages(&sent);

        let stats = fc.get_stats();
        assert_eq!(stats.tx_queue_size, 0);
    }

    #[test]
    fn test_throttling() {
        let mut config = FlowControlConfig::default();
        config.peer_reading_capacity = 1;
        config.peer_flood_reading_capacity = 1;

        let fc = FlowControl::new(config);
        let tx = make_tx_message();

        // Process message to consume capacity
        assert!(fc.begin_message_processing(&tx));

        // Should be throttled after consuming capacity
        assert!(fc.maybe_throttle_read());
        assert!(fc.is_throttled());

        // Stop throttling
        let duration = fc.stop_throttling();
        assert!(duration.is_some());
        assert!(!fc.is_throttled());
    }

    #[test]
    fn test_queue_trimming() {
        let mut config = FlowControlConfig::default();
        config.max_tx_set_size_ops = 5;
        config.outbound_tx_queue_byte_limit = 1000;

        let fc = FlowControl::new(config);

        // Grant capacity
        let send_more = StellarMessage::SendMoreExtended(SendMoreExtended {
            num_messages: 100,
            num_bytes: 10_000_000,
        });
        fc.maybe_release_capacity(&send_more);

        // Add many messages to trigger trimming
        for _ in 0..10 {
            fc.add_msg_and_maybe_trim_queue(make_tx_message());
        }

        // Queue should have been trimmed
        let stats = fc.get_stats();
        assert!(stats.tx_queue_size <= 5 || stats.tx_queue_size == 0);
    }

    fn make_scp_message_at_slot(slot: u64) -> StellarMessage {
        StellarMessage::ScpMessage(ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: stellar_xdr::curr::NodeId(
                    stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                        [0u8; 32],
                    )),
                ),
                slot_index: slot,
                pledges: stellar_xdr::curr::ScpStatementPledges::Nominate(
                    stellar_xdr::curr::ScpNomination {
                        quorum_set_hash: Hash([0u8; 32]),
                        votes: vec![].try_into().unwrap(),
                        accepted: vec![].try_into().unwrap(),
                    },
                ),
            },
            signature: stellar_xdr::curr::Signature::default(),
        })
    }

    struct MockScpCallback {
        min_slot: u64,
        checkpoint_seq: u64,
    }

    impl ScpQueueCallback for MockScpCallback {
        fn min_slot_to_remember(&self) -> u64 {
            self.min_slot
        }
        fn most_recent_checkpoint_seq(&self) -> u64 {
            self.checkpoint_seq
        }
    }

    #[test]
    fn test_scp_queue_trimming_drops_old_slots() {
        let mut config = FlowControlConfig::default();
        config.max_tx_set_size_ops = 3; // Trim when queue > 3

        let callback = Arc::new(MockScpCallback {
            min_slot: 100,
            checkpoint_seq: 50,
        });
        let fc = FlowControl::with_scp_callback(config, Some(callback));

        // Add messages at various slots
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(10)); // old, should drop
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(50)); // checkpoint, should keep
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(80)); // old, should drop
                                                                       // Queue is at 3 — not over limit yet

        // This 4th message triggers trimming (queue > limit of 3)
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(100)); // current, should keep

        let stats = fc.get_stats();
        // Slot 10 and 80 should have been dropped (below min_slot=100, not checkpoint=50)
        // Slot 50 and 100 should remain
        // But slot 10 is below min AND not checkpoint — dropped
        // Slot 80 is below min AND not checkpoint — dropped
        // Slot 50 IS the checkpoint seq — kept
        // Slot 100 is at min — kept
        assert_eq!(stats.scp_queue_size, 2);
    }

    #[test]
    fn test_scp_queue_trimming_preserves_checkpoint() {
        let mut config = FlowControlConfig::default();
        config.max_tx_set_size_ops = 2; // Trim when queue > 2

        let callback = Arc::new(MockScpCallback {
            min_slot: 100,
            checkpoint_seq: 50,
        });
        let fc = FlowControl::with_scp_callback(config, Some(callback));

        // Fill past limit with old slots including the checkpoint
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(50)); // checkpoint
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(10)); // old
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(200)); // triggers trim

        let stats = fc.get_stats();
        // Slot 10 should be dropped, slot 50 (checkpoint) and 200 should remain
        assert_eq!(stats.scp_queue_size, 2);
    }

    #[test]
    fn test_scp_queue_fallback_fifo_without_callback() {
        let mut config = FlowControlConfig::default();
        config.max_tx_set_size_ops = 3; // Trim when queue > 3

        // No callback — should use FIFO fallback
        let fc = FlowControl::new(config);

        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(1));
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(2));
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(3));
        fc.add_msg_and_maybe_trim_queue(make_scp_message_at_slot(4)); // triggers trim

        let stats = fc.get_stats();
        // FIFO trims to limit/2 = 1, then the new msg makes it 2
        // (the 4th message was already added before trimming happens)
        assert!(stats.scp_queue_size <= 3);
    }

    #[test]
    fn test_counting_writer_matches_xdr_len() {
        let tx = make_tx_message();
        let scp = make_scp_message();

        // CountingWriter-based size
        let tx_size = msg_body_size(&tx);
        let scp_size = msg_body_size(&scp);

        // Vec-based size (the old approach)
        let tx_xdr = tx.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        let scp_xdr = scp.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();

        assert_eq!(tx_size, tx_xdr.len() as u64);
        assert_eq!(scp_size, scp_xdr.len() as u64);
        assert!(tx_size > 0);
        assert!(scp_size > 0);
    }

    #[test]
    fn test_capacity_guard_finish_returns_send_more() {
        let fc = Arc::new(FlowControl::default());
        let tx = make_tx_message();

        // Process enough messages to trigger a SEND_MORE batch
        let batch = fc.config.flow_control_send_more_batch_size;
        for _ in 0..batch {
            let guard = CapacityGuard::new(Arc::clone(&fc), tx.clone()).unwrap();
            let cap = guard.finish();
            // Only the last one (or once threshold is met) should trigger
            if cap.should_send() {
                assert!(cap.num_flood_messages > 0);
            }
        }
    }

    #[test]
    fn test_capacity_guard_drop_releases_capacity() {
        let mut config = FlowControlConfig::default();
        config.peer_reading_capacity = 2;
        config.peer_flood_reading_capacity = 2;
        let fc = Arc::new(FlowControl::new(config));
        let tx = make_tx_message();

        // Lock capacity with a guard, then drop without finish()
        {
            let _guard = CapacityGuard::new(Arc::clone(&fc), tx.clone()).unwrap();
            // Guard dropped here — capacity should be released
        }

        // We should still be able to create another guard (capacity was released)
        let guard2 = CapacityGuard::new(Arc::clone(&fc), tx.clone());
        assert!(
            guard2.is_some(),
            "capacity should have been released by Drop"
        );
    }

    #[test]
    fn test_capacity_guard_none_when_no_capacity() {
        let mut config = FlowControlConfig::default();
        config.peer_reading_capacity = 1;
        config.peer_flood_reading_capacity = 1;
        let fc = Arc::new(FlowControl::new(config));
        let tx = make_tx_message();

        // First guard takes the only capacity slot
        let guard1 = CapacityGuard::new(Arc::clone(&fc), tx.clone());
        assert!(guard1.is_some());

        // Second guard should fail — no capacity
        let guard2 = CapacityGuard::new(Arc::clone(&fc), tx.clone());
        assert!(guard2.is_none(), "should fail when no capacity");

        // Drop first guard — capacity returns
        drop(guard1);

        // Now should succeed again
        let guard3 = CapacityGuard::new(Arc::clone(&fc), tx.clone());
        assert!(guard3.is_some());
    }

    #[test]
    fn test_capacity_guard_finish_prevents_double_release() {
        let fc = Arc::new(FlowControl::default());
        let tx = make_tx_message();

        let initial_stats = fc.get_stats();

        let guard = CapacityGuard::new(Arc::clone(&fc), tx.clone()).unwrap();
        let _cap = guard.finish(); // consumes guard, calls end once

        // After finish + implicit drop (no-op), capacity should match
        // one begin + one end cycle
        let final_stats = fc.get_stats();
        assert_eq!(
            initial_stats.local_flood_capacity, final_stats.local_flood_capacity,
            "capacity should be fully restored after finish"
        );
    }

    /// Regression test for AUDIT-H9: when byte capacity is exhausted but
    /// message capacity is available, a failed `begin_message_processing`
    /// must not leak message capacity.
    ///
    /// Before the fix, `lock_local_capacity` both checked and deducted in
    /// one call: message_capacity was deducted first, then byte_capacity
    /// check failed, but message_capacity was never rolled back.
    #[test]
    fn test_begin_message_processing_no_capacity_leak_on_byte_failure() {
        // Set byte capacity to 1 byte — any real tx message is larger than
        // 1 byte, so byte capacity will always reject.
        let config = FlowControlConfig {
            flow_control_bytes_batch_size: 1,
            ..FlowControlConfig::default()
        };
        let fc = FlowControl::new(config);
        let tx = make_tx_message();

        let before = fc.get_stats();
        assert!(
            before.local_flood_capacity > 0,
            "precondition: message flood capacity should be available"
        );

        // This should fail because byte capacity (1) < tx byte size.
        let accepted = fc.begin_message_processing(&tx);
        assert!(
            !accepted,
            "should be rejected due to insufficient byte capacity"
        );

        let after = fc.get_stats();
        assert_eq!(
            before.local_flood_capacity, after.local_flood_capacity,
            "AUDIT-H9: message flood capacity must not leak when byte capacity check fails"
        );
        assert_eq!(
            before.local_total_capacity, after.local_total_capacity,
            "AUDIT-H9: message total capacity must not leak when byte capacity check fails"
        );
    }

    /// Symmetric test: when message capacity is exhausted but byte capacity
    /// is available, a failed `begin_message_processing` must not leak byte
    /// capacity.
    #[test]
    fn test_begin_message_processing_no_capacity_leak_on_message_failure() {
        // Set message capacity to 0 flood messages — will always reject.
        let config = FlowControlConfig {
            peer_flood_reading_capacity: 0,
            peer_reading_capacity: 1,
            ..FlowControlConfig::default()
        };
        let fc = FlowControl::new(config);
        let tx = make_tx_message();

        let before = fc.get_stats();

        // This should fail because flood_capacity (0) < 1 (message cost).
        let accepted = fc.begin_message_processing(&tx);
        assert!(
            !accepted,
            "should be rejected due to insufficient message flood capacity"
        );

        let after = fc.get_stats();
        assert_eq!(
            before.local_flood_bytes_capacity, after.local_flood_bytes_capacity,
            "AUDIT-H9: byte flood capacity must not leak when message capacity check fails"
        );
    }
}
