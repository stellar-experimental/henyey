# overlay

The overlay crate implements henyey's peer-to-peer networking layer. It manages authenticated TCP connections between nodes, handles message framing and flow control, coordinates flood-based message propagation, and supports network topology surveys.

## Key Files

- [lib.pc.md](lib.pc.md) — Top-level crate module with configuration types and local node identity
- [manager.pc.md](manager.pc.md) — Overlay orchestrator managing peer connections, timeouts, and message routing
- [peer.pc.md](peer.pc.md) — Peer state machine (Connecting, Handshaking, Authenticated, Closing, Disconnected)
- [auth.pc.md](auth.pc.md) — X25519 key exchange with HMAC-SHA256 message authentication
- [flow_control.pc.md](flow_control.pc.md) — Per-peer flow control with capacity tracking and SEND_MORE back-pressure
- [flood.pc.md](flood.pc.md) — Flood gate for message propagation with duplicate detection and TTL expiry
- [connection.pc.md](connection.pc.md) — Low-level TCP transport with framed connections and connection pool

## Architecture

The overlay is organized around `OverlayManager` (in `manager.pc.md`), which orchestrates peer connections and message routing. Each peer is represented by a `Peer` state machine backed by `Connection` (transport) and `Auth` (authenticated key exchange). Flow control (`FlowControl`) applies back-pressure via message-count and byte-count capacity tracking. Transaction propagation uses a pull-mode protocol with FloodAdvert/FloodDemand transport handled here (scheduling owned by app crate), while SCP items are fetched on-demand via `ItemFetcher`. The `Survey` module enables network-wide topology data collection. `MessageHandlers` dispatches incoming messages to the appropriate subsystem, and `Metrics` tracks all overlay counters and timers.

## All Files

| File | Description |
|------|-------------|
| [auth.pc.md](auth.pc.md) | X25519 key exchange and HMAC-SHA256 message authentication |
| [ban_manager.pc.md](ban_manager.pc.md) | SQLite-backed persistent peer ban list management |
| [codec.pc.md](codec.pc.md) | Wire-format message codec with 4-byte length-prefixed XDR framing |
| [connection.pc.md](connection.pc.md) | Low-level TCP connection handling with framing and connection pool |
| [error.pc.md](error.pc.md) | Error type enumeration for overlay network operations |
| [flood.pc.md](flood.pc.md) | Flood gate for message propagation with duplicate detection |
| [flow_control.pc.md](flow_control.pc.md) | Per-peer flow control with capacity tracking and back-pressure |
| [item_fetcher.pc.md](item_fetcher.pc.md) | Hash-based fetch tracking for TxSet and QuorumSet retrieval |
| [lib.pc.md](lib.pc.md) | Top-level crate module with configuration and local node identity |
| [manager.pc.md](manager.pc.md) | Overlay orchestrator for peer lifecycle, timeouts, and routing |
| [message_handlers.pc.md](message_handlers.pc.md) | Message dispatcher routing incoming messages to subsystems |
| [metrics.pc.md](metrics.pc.md) | Overlay metric counters, timers, and histograms |
| [peer.pc.md](peer.pc.md) | Peer state machine with lifecycle and connection metadata |
| [peer_manager.pc.md](peer_manager.pc.md) | Peer address persistence with failure tracking and back-off |
| [survey.pc.md](survey.pc.md) | Network topology survey with phase-based lifecycle |
