# Overlay Guard Checklist

**Upstream reference**: `stellar-core/src/overlay/Peer.cpp`, `Peer.h`, `OverlayManagerImpl.cpp`
**Last updated**: 2025-07-29

This checklist enumerates every guard clause that stellar-core applies at overlay
message handler entry points, cross-referenced with Henyey's implementation status.

## Summary

| Status | Count |
|--------|-------|
| PRESENT | 20 |
| **Total** | **20** |

## Frame Layer (pre-dispatch)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Message size limit (MAX_MESSAGE_SIZE) | `Peer.h:26-31` | `codec.rs` frame length check | PRESENT | |
| MAC / HMAC verification on authenticated messages | `Peer.cpp:1178-1186` (CryptoError catch) | `auth.rs:666` (`compute_mac`) | PRESENT | |

## Pre-auth Filtering

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Pre-handshake message type filter (only HELLO/AUTH/ERROR allowed) | `Peer.cpp:1225-1233` (`recvRawMessage`) | `connection.rs` auth state machine | PRESENT | |
| Post-auth handshake rejection (drop if Hello/Auth sent after auth) | `Peer.cpp:1225-1233` | `peer_loop.rs:422` (`is_handshake_message` check) | PRESENT | |

## Top-level Message Dispatch (`recvMessage`)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| `shouldAbort` (peer/overlay shutting down) | `Peer.cpp:1157-1160` | `peer_loop.rs` (`running` atomic + early check in `route_received_message`) | PRESENT | |
| `ignoreIfOutOfSync` — Transaction | `Peer.cpp:1164-1172` | Herder state gate (`can_receive_transactions`) | PRESENT | |
| `ignoreIfOutOfSync` — FloodAdvert | `Peer.cpp:1164-1172` | `lifecycle.rs` (`is_tracking()` guard) | PRESENT | |
| `ignoreIfOutOfSync` — FloodDemand | `Peer.cpp:1164-1172` | `lifecycle.rs` (`is_tracking()` guard) | PRESENT | |

## PEERS Message

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Direction check (inbound peers only) | `Peer.cpp:1235-1240` | `peer_loop.rs:401` (`validate_incoming_peers`) | PRESENT | |
| Duplicate PEERS guard (one per peer) | `Peer.cpp:1993-1998` | `peer_loop.rs:401` (`RejectDuplicate`) | PRESENT | |
| Port validation (`port != 0`) | `Peer.cpp:2003-2008` | `peer_loop.rs` (`validate_incoming_peers`) | PRESENT | |
| Private address filtering | `Peer.cpp:2019-2023` | `peer_loop.rs` (`validate_incoming_peers`) | PRESENT | |
| Self-address filtering | `Peer.cpp:2024-2030` | `peer_loop.rs` (`validate_incoming_peers`) | PRESENT | |

## Flow Control

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| SEND_MORE validity check | `Peer.cpp:1384-1390` | `flow_control.rs:758` (`is_send_more_valid`) | PRESENT | |
| AUTH flow control flags validation | `Peer.cpp:1959-1963` | `connection.rs` (AUTH handling) | PRESENT | |

## Flood Message Routing

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Floodgate dedup (hash-based) | `Peer.cpp` broadcast path | `peer_loop.rs:448-452` (`compute_message_hash` + `record_seen`) | PRESENT | |
| Watcher filter (non-validators drop flood msgs) | `OverlayManagerImpl.cpp` | `peer_loop.rs:435` (`is_watcher_droppable`) | PRESENT | |
| Global rate limiter | N/A (henyey-specific) | `peer_loop.rs:441` (`flood_gate.allow_message`) | PRESENT | |

## Query Messages (GetTxSet / GetScpQuorumSet / GetSCPState)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Sliding-window query rate limit | `Peer.cpp:1423-1438` (`process()`) | `peer_loop.rs` (`QueryRateLimiter`) | PRESENT | |

## Timeouts

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Idle timeout (30s) | `Peer.cpp` | `peer_loop.rs:277` (`PEER_TIMEOUT`) | PRESENT | |
| Straggler timeout (120s) | `Peer.cpp` | `peer_loop.rs:278` (`PEER_STRAGGLER_TIMEOUT`) | PRESENT | |
| Send-mode idle timeout (60s) | `Peer.cpp` | `peer_loop.rs:280` (`PEER_SEND_MODE_IDLE_TIMEOUT_SECS`) | PRESENT | |
