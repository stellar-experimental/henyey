# H-023: Outbound Channel Backpressure Bypassed by Direct Peer::send

**Date**: 2025-01-27
**Crate**: overlay
**Severity**: MEDIUM
**Hypothesis by**: claude-opus-4.6

## Expected Behavior
All messages sent to a peer should respect flow control capacity to prevent
the local node from overwhelming a slow or malicious peer's TCP receive buffer.
A peer that stops reading should trigger backpressure on the sender.

## Mechanism
The overlay has two sending paths:

1. **Flow-controlled path** (`FlowControl::add_msg_and_maybe_trim_queue` →
   `get_next_batch_to_send` → `send_flow_controlled_batch`): Respects both
   message and byte capacity. Used for flood messages (SCP, TX, adverts, demands).

2. **Direct path** (`Peer::send` / `peer.send_more_extended` / `maybe_send_ping`):
   Bypasses flow control entirely and writes directly to the TCP socket via
   `Connection::send` with a 10-second timeout.

Messages sent via the direct path include:
- `SendMoreExtended` (flow control grants)
- `GetScpQuorumset` (ping/RTT measurement)
- `ErrorMsg` (peer disconnect)
- `GetScpState` (initial sync after auth)
- `Peers` (peer list)

While individual direct sends have a 10s timeout (connection.rs:196), the
cumulative effect of these sends is not rate-limited. A malicious peer that
deliberately reads slowly (but not zero-rate) can cause the 10s timeout to
almost-but-not-quite fire on each direct send, creating up to 10 seconds of
blocking per ping/SendMore cycle.

The per-peer outbound message channel (256 slots, peer_loop.rs:31) provides
buffering for the flow-controlled path, but direct sends from the peer loop
task itself block that task. If a direct send blocks for close to 10 seconds,
the periodic tick timer (1s interval) accumulates missed ticks. Upon unblocking,
multiple pings may fire in rapid succession.

More critically, if the peer loop task is blocked on a direct send for 10
seconds, it cannot process incoming messages from that peer during that time.
Combined with the 30-second idle timeout, a slow-reading peer has a ~10/30
second ratio of induced blocking before being dropped.

## Attack Vector
1. Attacker opens authenticated inbound connections (up to slot limit)
2. Attacker reads from TCP socket at an extremely low rate (e.g., 1 byte/second)
3. The victim's `Connection::send` blocks on TCP backpressure for each message
4. The 10-second send timeout eventually fires, but the victim's peer loop is
   blocked during that time
5. During the 10s block, the victim cannot process any other messages from this
   peer (including SCP messages that may be time-critical)
6. With N attacker connections each blocking for ~10s per send cycle, the victim's
   peer task threads are occupied, potentially impacting throughput

## Target Code
- `crates/overlay/src/connection.rs:185-215` — `send()` with 10s timeout
- `crates/overlay/src/manager/peer_loop.rs:964-966` — `maybe_send_ping` blocks
- `crates/overlay/src/manager/peer_loop.rs:1083-1088` — `send_more_extended` blocks
- `crates/overlay/src/peer.rs:652-670` — `Peer::send` is a direct TCP write

## Evidence
- `Connection::send` uses a 10-second timeout (connection.rs:195-196)
- `maybe_send_ping` calls `peer.send()` directly (peer_loop.rs:601)
- `send_more_extended` calls `peer.send()` directly (peer.rs:817-825)
- The peer loop is single-threaded per-peer — a blocked send blocks all processing
- TCP backpressure from a slow reader naturally causes write blocking
- Each peer loop runs in its own tokio task, so only that peer is affected

## Anti-Evidence
- Each peer has its own independent tokio task — one slow peer doesn't block others
- The 10-second timeout provides an upper bound on blocking per send
- After timeout, the connection is closed (`self.closed = true` at line 208)
- Tokio tasks are lightweight and the runtime can handle thousands of them
- This attack only slows down communication with the attacker's own connection
- The attacker wastes their own authenticated slot in the process
- Production networks have many peers so losing one slot has limited impact

---
## Review
**Verdict**: NOT_VIABLE
**Failed At**: hypothesis
**Reviewed by**: claude-opus-4.6
### Why It Failed
The attack is self-defeating: each slow-reader connection only blocks its own
peer loop task. The victim's communication with all other peers remains
unaffected (separate tokio tasks). After 10 seconds the connection is
forcibly closed. The attacker burns one authenticated slot to delay their own
messages for 10 seconds before being disconnected. With limited inbound slots,
this doesn't create meaningful impact. The per-task isolation model makes this
a non-issue.

### Lesson Learned
Per-connection task isolation (one tokio task per peer loop) naturally limits
the blast radius of slow-consumer attacks. The 10-second send timeout is the
effective defense. Vulnerabilities require cross-peer impact to be meaningful.
