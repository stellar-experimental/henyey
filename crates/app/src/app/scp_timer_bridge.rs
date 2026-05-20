//! Bridge between the herder's [`TimerManager`] and the app's main event loop.
//!
//! The [`TimerManager`] fires callbacks via the [`TimerCallback`] trait when
//! SCP nomination/ballot timers expire. This module provides a channel-based
//! adapter that sends timer events to the main event loop for processing,
//! matching stellar-core's single-shot `VirtualTimer` delivery pattern.
//!
//! Each event is stamped with the tracking epoch that was current when the
//! timer was scheduled (captured by [`TimerManager`] at schedule-time). This
//! allows `handle_scp_timer_event()` to discard stale events from prior epochs
//! even if they expire after `on_lost_sync()` increments the epoch.

use henyey_herder::{TimerCallback, TimerType};
use henyey_scp::SlotIndex;

/// An SCP timer event delivered from the [`TimerManager`] to the main loop.
#[derive(Debug, Clone)]
pub(super) struct ScpTimerEvent {
    pub slot: SlotIndex,
    pub timer_type: TimerType,
    /// The tracking epoch at which this timer was originally scheduled.
    pub epoch: u64,
}

/// Adapter implementing [`TimerCallback`] that forwards timer fires over a channel.
pub(super) struct ScpTimerBridge {
    sender: tokio::sync::mpsc::UnboundedSender<ScpTimerEvent>,
}

impl ScpTimerBridge {
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<ScpTimerEvent>) -> Self {
        Self { sender }
    }
}

impl TimerCallback for ScpTimerBridge {
    fn on_nomination_timeout(&self, slot: SlotIndex, epoch: u64) {
        let _ = self.sender.send(ScpTimerEvent {
            slot,
            timer_type: TimerType::Nomination,
            epoch,
        });
    }

    fn on_ballot_timeout(&self, slot: SlotIndex, epoch: u64) {
        let _ = self.sender.send(ScpTimerEvent {
            slot,
            timer_type: TimerType::Ballot,
            epoch,
        });
    }
}
