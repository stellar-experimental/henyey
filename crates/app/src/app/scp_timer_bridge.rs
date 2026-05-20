//! Bridge between the herder's [`TimerManager`] and the app's main event loop.
//!
//! The [`TimerManager`] fires callbacks via the [`TimerCallback`] trait when
//! SCP nomination/ballot timers expire. This module provides a channel-based
//! adapter that sends timer events to the main event loop for processing,
//! matching stellar-core's single-shot `VirtualTimer` delivery pattern.
//!
//! Each event is stamped with a tracking epoch so the receiver can discard
//! stale events that were queued before a sync-loss transition. The epoch is
//! incremented by `on_lost_sync()` and checked by `handle_scp_timer_event()`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use henyey_herder::{TimerCallback, TimerType};
use henyey_scp::SlotIndex;

/// An SCP timer event delivered from the [`TimerManager`] to the main loop.
#[derive(Debug, Clone)]
pub(super) struct ScpTimerEvent {
    pub slot: SlotIndex,
    pub timer_type: TimerType,
    /// The tracking epoch at which this event was enqueued.
    pub epoch: u64,
}

/// Adapter implementing [`TimerCallback`] that forwards timer fires over a channel.
pub(super) struct ScpTimerBridge {
    sender: tokio::sync::mpsc::UnboundedSender<ScpTimerEvent>,
    /// Shared tracking epoch — incremented on sync loss.
    epoch: Arc<AtomicU64>,
}

impl ScpTimerBridge {
    pub fn new(
        sender: tokio::sync::mpsc::UnboundedSender<ScpTimerEvent>,
        epoch: Arc<AtomicU64>,
    ) -> Self {
        Self { sender, epoch }
    }
}

impl TimerCallback for ScpTimerBridge {
    fn on_nomination_timeout(&self, slot: SlotIndex) {
        let _ = self.sender.send(ScpTimerEvent {
            slot,
            timer_type: TimerType::Nomination,
            epoch: self.epoch.load(Ordering::Acquire),
        });
    }

    fn on_ballot_timeout(&self, slot: SlotIndex) {
        let _ = self.sender.send(ScpTimerEvent {
            slot,
            timer_type: TimerType::Ballot,
            epoch: self.epoch.load(Ordering::Acquire),
        });
    }
}
