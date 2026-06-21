# Henyey Overlay Crate — Specification Adherence Evaluation

**Evaluated against:** `stellar-specs/OVERLAY_SPEC.md` (Stellar Overlay Protocol Specification v26)
**Upstream reference:** stellar-core v26.0.1
**stellar-core pin:** v27.0.0 — evaluation body reflects the v26.0.1 baseline; Protocol 27 delta re-eval pending.
**Crate:** `crates/overlay/` (henyey-overlay)
**Date:** 2026-05-10

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Evaluation Methodology](#2-evaluation-methodology)
3. [Section-by-Section Evaluation](#3-section-by-section-evaluation)
   - [§3 Data Encoding](#33-data-encoding-spec-3)
   - [§4 Connection Lifecycle](#34-connection-lifecycle-spec-4)
   - [§5 Message Framing and Authentication](#35-message-framing-and-authentication-spec-5)
   - [§6 Message Type Registry](#36-message-type-registry-spec-6)
   - [§7 Message Definitions](#37-message-definitions-spec-7)
   - [§8 Flow Control Protocol](#38-flow-control-protocol-spec-8)
   - [§9 Transaction Flooding Protocol](#39-transaction-flooding-protocol-spec-9)
   - [§10 Peer Management](#310-peer-management-spec-10)
   - [§11 Survey Protocol](#311-survey-protocol-spec-11)
   - [§12 Error Handling](#312-error-handling-spec-12)
   - [§13 Security Considerations](#313-security-considerations-spec-13)
   - [§14 Protocol Constants](#314-protocol-constants-spec-14)
4. [Gap Summary](#4-gap-summary)
5. [Risk Assessment](#5-risk-assessment)
6. [v26.0.1 Implementation Delta](#6-v2601-implementation-delta)
7. [Recommendations](#7-recommendations)

---

## 1. Executive Summary

The henyey overlay crate implements a substantial portion of the Stellar overlay protocol as specified in `OVERLAY_SPEC.md`. The core protocol machinery — TCP connection management, HELLO/AUTH handshake, Curve25519 key exchange, HMAC-SHA256 message authentication, dual-axis flow control, pull-mode transaction flooding, and SQLite-backed peer persistence — is present and structurally correct.

### Overall Adherence Rating

| Category | Rating | Notes |
|----------|--------|-------|
| **Connection Lifecycle** | **Full** | Full handshake, self-connection rejection, version validation, 2s auth timeout, pending peer tracking |
| **Message Framing & Auth** | **Full** | RFC 5531 framing, ECDH, HKDF, HMAC-SHA256, bit-31 auth flag |
| **Flow Control** | **Full** | Dual-axis capacity, 4-priority queuing, SEND_MORE_EXTENDED, CapacityGuard RAII, per-peer enforcement |
| **Transaction Flooding** | **Full** | Pull-mode with advert batching, demand scheduling, retry backoff |
| **Peer Management** | **Full** | SQLite persistence, 3s tick loop, DNS re-resolution, random peer rotation, dead peer purge, config peer storage |
| **Survey Protocol** | **Full** | Time-sliced lifecycle, Ed25519 signing, Curve25519 sealed-box encryption |
| **Error Handling** | **Full** | ERR_LOAD load shedding, 100-byte message cap, auto-ban escalation |
| **Protocol Constants** | **Full** | All critical constants match stellar-core v26.0.1 values |

**Estimated specification coverage: ~100%** of MUST/SHALL requirements are implemented. All 17 gaps identified in the original evaluation have been closed.

---

## 2. Evaluation Methodology

This evaluation compares the henyey overlay implementation file-by-file against the formal specification. Each spec section is assessed on three dimensions:

1. **Structural completeness**: Are the required data structures, messages, and state machines present?
2. **Behavioral correctness**: Do the implementations follow the specified algorithms and state transitions?
3. **Constant fidelity**: Do hardcoded values, thresholds, and timeouts match the spec?

Ratings per requirement:

| Symbol | Meaning |
|--------|---------|
| ✅ | Fully implemented and matches spec |
| ⚠️ | Partially implemented or minor deviation |
| ❌ | Not implemented |
| ➖ | Not applicable to henyey's architecture |

Source file references use the format `file.rs:line`.

---

## 3. Section-by-Section Evaluation

### 3.3 Data Encoding (Spec §3)

**Source files:** `codec.rs`, `auth.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| XDR encoding per RFC 4506 | ✅ | Uses `stellar_xdr` crate for all serialization (`codec.rs:1`) |
| Big-endian byte ordering | ✅ | Handled by the XDR crate |
| `StellarMessage` as top-level union | ✅ | All messages encoded/decoded as `StellarMessage` variants |
| `AuthenticatedMessage` wrapping | ✅ | `AuthenticatedMessageV0` with sequence + HMAC (`codec.rs:180-220`) |
| 32-byte HMAC-SHA256 MACs | ✅ | `HmacSha256Mac` correctly sized (`auth.rs:400+`) |
| Ed25519 signatures 64 bytes | ✅ | Standard Ed25519 via `ed25519-dalek` |
| Curve25519 public keys 32 bytes | ✅ | X25519 via `x25519-dalek` (`auth.rs:80+`) |
| SHA-256 hashes 32 bytes | ✅ | Via `sha2` crate |

**Assessment: Full adherence.** The XDR encoding layer is delegated to the well-tested `stellar_xdr` crate, which correctly implements RFC 4506.

---

### 3.4 Connection Lifecycle (Spec §4)

**Source files:** `peer.rs`, `connection.rs`, `manager.rs`, `lib.rs`

#### 4.1 TCP Transport

| Requirement | Status | Evidence |
|-------------|--------|----------|
| TCP as transport | ✅ | `TcpStream` used throughout (`connection.rs:30+`) |
| Default port 11625 (pubnet) / 11626 (testnet) | ✅ | Configurable via `OverlayConfig` (`lib.rs:50+`) |
| `TCP_NODELAY` set | ✅ | `stream.set_nodelay(true)` (`connection.rs:85`) |
| Connection timeout for pending peers | ✅ | 2s `PEER_AUTHENTICATION_TIMEOUT` enforced (`lib.rs`) |

#### 4.2 Connection Limits

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `MAX_ADDITIONAL_PEER_CONNECTIONS` (64) default | ✅ | `max_inbound_connections: 64` (`lib.rs:65`) |
| `TARGET_PEER_CONNECTIONS` (8) default | ✅ | `target_outbound_connections: 8` (`lib.rs:60`) |
| Preferred peers get extra slots | ✅ | `max_preferred_connections` separate from main limit (`manager.rs:250+`) |
| Reject inbound when at capacity | ✅ | Checked in `handle_inbound_connection` (`manager.rs:400+`) |
| Pending peer tracking | ✅ | `ConnectionPool` with `pending_count`/`authenticated_count` atomics and `mark_authenticated()`/`release_*()` (`manager.rs`) |

#### 4.3 Handshake State Machine

| Requirement | Status | Evidence |
|-------------|--------|----------|
| 4-step: HELLO→HELLO→AUTH→AUTH | ✅ | `perform_handshake` in `peer.rs:150-300` |
| Initiator sends HELLO first | ✅ | `send_hello` called first for `WE_CALLED_REMOTE` (`peer.rs:160`) |
| Responder sends HELLO after receiving | ✅ | `recv_hello` then `send_hello` for `REMOTE_CALLED_US` (`peer.rs:170`) |
| Initiator sends AUTH after receiving HELLO | ✅ | Sequence enforced in handshake flow |
| Responder sends AUTH after receiving AUTH | ✅ | Sequence enforced in handshake flow |
| `PEER_AUTHENTICATION_TIMEOUT` (2s) | ✅ | Default `auth_timeout_secs` set to 2 (`lib.rs`) |

#### 4.4 Hello Message Validation

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Reject if `overlayVersion < MIN_OVERLAY_VERSION` | ✅ | Version check in `validate_hello` (`peer.rs:320+`) |
| Reject if `networkID` mismatch | ✅ | Network ID compared (`peer.rs:330`) |
| Reject self-connections via nonce | ✅ | `is_self_connection` check (`peer.rs:340`) |
| Reject if peer already connected (by NodeID) | ✅ | Duplicate detection in `manager.rs:450+` |
| Validate `listeningPort` > 0 | ✅ | Port validated during handshake |
| Validate AuthCert expiration | ✅ | Expiry checked against current time (`auth.rs:300+`) |
| Validate AuthCert signature | ✅ | Ed25519 signature verification (`auth.rs:310+`) |
| Update peer address from Hello | ✅ | Remote address recorded from Hello message (`peer.rs:350`) |

#### 4.5 Ping/Pong Keepalive

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Periodic keepalive mechanism | ✅ | Synthetic `GetScpQuorumset` with random hash as ping (`manager.rs:800+`) |
| Ping/pong RTT measurement | ✅ | RTT measured from `DontHave`/`ScpQuorumset` response (`manager.rs`) |
| Latency-based peer quality | ✅ | Latency tracked per-peer |
| Idle timeout disconnect | ✅ | Dead peer detection in tick loop |

**Assessment: Full adherence.** The 4-step handshake is correctly sequenced with proper auth timeout, pending peer tracking, and keepalive via synthetic `GetScpQuorumset` matching stellar-core's `Peer::pingPeer()` approach.

---

### 3.5 Message Framing and Authentication (Spec §5)

**Source files:** `codec.rs`, `auth.rs`

#### 5.1 Record Marking (RFC 5531)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| 4-byte big-endian length prefix | ✅ | `codec.rs:100-130`, length read as `u32::from_be_bytes` |
| Bit 31 = 1 for last fragment (always set) | ✅ | Auth flag in bit 31 (`codec.rs:115`) |
| No multi-fragment support needed | ✅ | Single-fragment messages only |
| `MAX_MESSAGE_SIZE` = 0x2000000 (32 MB) | ✅ | `MAX_MESSAGE_SIZE: u32 = 0x200_0000` (`codec.rs:20`) |
| `MIN_MESSAGE_SIZE` = 12 bytes | ✅ | Minimum size enforced (`codec.rs:22`) |
| Reject oversized messages | ✅ | Size check before allocation (`codec.rs:135`) |

#### 5.2 Authentication Bit Semantics

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Bit 31 clear → unauthenticated (Hello only) | ✅ | Hello sent without auth bit (`codec.rs:180`) |
| Bit 31 set → authenticated message | ✅ | Auth bit set for post-handshake messages (`codec.rs:190`) |
| Reject auth messages before handshake complete | ✅ | State-checked in codec decode path |
| Auth flags field uses bit 31 (`0x80000000`) | ✅ | Uses bit 31 with clarifying comment (`codec.rs`) |

#### 5.3 Key Derivation

| Requirement | Status | Evidence |
|-------------|--------|----------|
| X25519 ECDH shared secret | ✅ | `x25519_dalek::diffie_hellman` (`auth.rs:200+`) |
| HKDF-Extract with `SHA-256(empty)` as salt | ✅ | `hkdf::Hkdf::<Sha256>::new(None, ...)` (`auth.rs:220`) |
| HKDF-Expand with direction-differentiated info | ✅ | `"sending"` vs `"receiving"` labels based on role (`auth.rs:230-240`) |
| Separate send/receive keys | ✅ | `sending_mac_key` and `receiving_mac_key` (`auth.rs:250`) |
| Direction swapped for initiator vs responder | ✅ | Role-based key assignment (`auth.rs:260`) |

#### 5.4 HMAC-SHA256 Message Authentication

| Requirement | Status | Evidence |
|-------------|--------|----------|
| HMAC-SHA256 over `(sequence ∥ message_xdr)` | ✅ | MAC computed over sequence number concatenated with message bytes (`auth.rs:400+`) |
| Monotonically increasing sequence numbers | ✅ | `send_sequence` incremented per message (`auth.rs:420`) |
| Reject out-of-sequence messages | ✅ | Sequence validation in `verify_mac` (`auth.rs:430`) |
| Zero MAC for unauthenticated messages | ✅ | Hello messages have zeroed MAC field (`auth.rs:440`) |

**Assessment: Full adherence on framing and cryptography.** The ECDH + HKDF + HMAC pipeline is correctly implemented with proper direction differentiation.

---

### 3.6 Message Type Registry (Spec §6)

**Source files:** Uses `stellar_xdr::MessageType` enum (external crate)

| Message Type | Handled | Evidence |
|-------------|---------|----------|
| `ERROR_MSG` (0) | ✅ | Sent on disconnect (`manager.rs`), received and logged |
| `AUTH` (2) | ✅ | Handshake (`peer.rs`) |
| `DONT_HAVE` (3) | ✅ | Handled in message dispatch (`message_handlers.rs:100+`) |
| `GET_PEERS` (4) | ✅ | Response with peer list (`manager.rs:600+`) |
| `PEERS` (5) | ✅ | Peer list received and stored (`manager.rs:620+`) |
| `GET_TX_SET` (6) | ✅ | Dispatched to handler (`message_handlers.rs:120+`) |
| `TX_SET` (7) | ✅ | Received, forwarded to ledger subsystem |
| `GENERALIZED_TX_SET` (8) | ✅ | Handled alongside TX_SET |
| `TRANSACTION` (9) | ✅ | Core flooding path (`flood.rs`, `manager.rs`) |
| `GET_SCP_QUORUM` (11) | ✅ | Used as synthetic keepalive |
| `SCP_QUORUM` (12) | ✅ | Handled in dispatch |
| `SCP_MESSAGE` (13) | ✅ | Core SCP path (`manager.rs:700+`) |
| `GET_SCP_STATE` (14) | ✅ | Dispatched with rate limiting (10/min per peer) |
| `HELLO` (15) | ✅ | Handshake (`peer.rs`) |
| `SURVEY_REQUEST` (16) | ✅ | Survey subsystem (`survey.rs`) |
| `SURVEY_RESPONSE` (17) | ✅ | Survey subsystem (`survey.rs`) |
| `SEND_MORE` (18) | ✅ | Flow control (`flow_control.rs`) |
| `SEND_MORE_EXTENDED` (19) | ✅ | Flow control with byte capacity (`flow_control.rs`) |
| `FLOOD_ADVERT` (20) | ✅ | Pull-mode flooding (app `tx_flooding.rs`) |
| `FLOOD_DEMAND` (21) | ✅ | Pull-mode flooding (app `tx_flooding.rs`) |
| `TIME_SLICED_SURVEY_REQUEST` (22) | ✅ | Survey subsystem (`survey.rs`) |
| `TIME_SLICED_SURVEY_RESPONSE` (23) | ✅ | Survey subsystem (`survey.rs`) |
| `TIME_SLICED_SURVEY_START_COLLECTING` (24) | ✅ | Survey subsystem (`survey.rs`) |
| `TIME_SLICED_SURVEY_STOP_COLLECTING` (25) | ✅ | Survey subsystem (`survey.rs`) |

**Assessment: Full coverage.** All 25 message types in the spec are recognized and dispatched. The XDR crate provides the type definitions; henyey provides the handling logic.

---

### 3.7 Message Definitions (Spec §7)

**Source files:** `peer.rs`, `auth.rs`, `manager.rs`, `message_handlers.rs`

#### 7.1 Hello Message

| Field/Requirement | Status | Evidence |
|-------------------|--------|----------|
| `ledgerVersion` populated | ✅ | From current ledger state (`peer.rs:200`) |
| `overlayVersion` populated | ✅ | From config (`peer.rs:202`) |
| `overlayMinVersion` populated | ✅ | Set to minimum supported (`peer.rs:204`) |
| `networkID` = SHA-256(passphrase) | ✅ | Network ID from config (`peer.rs:206`) |
| `versionStr` ≤ 100 bytes | ✅ | Truncated if needed (`peer.rs:208`) |
| `listeningPort` from config | ✅ | From `OverlayConfig` (`peer.rs:210`) |
| `peerID` = node's Ed25519 public key | ✅ | From `LocalNode` (`peer.rs:212`) |
| `cert` = valid AuthCert | ✅ | Generated or cached AuthCert (`auth.rs:100+`) |
| `nonce` = 32 random bytes | ✅ | `rand::random()` nonce (`peer.rs:214`) |

#### 7.2 Auth Message

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Auth message is empty (flags = 0) | ✅ | `Auth { flags: 0 }` sent (`peer.rs:250`) |
| Unused field, value ignored | ✅ | Field not inspected on receipt |

#### 7.3 Error Message

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `ErrorCode` enum used | ✅ | XDR `ErrorCode` variants (`error.rs`) |
| `msg` string ≤ 100 chars | ✅ | `truncate_error_msg()`, `make_error_msg()` helpers cap at 100 bytes (`manager.rs`) |
| Sent before disconnect | ✅ | `send_error_and_disconnect` pattern (`manager.rs:500+`) |

#### 7.4 AuthCert

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Curve25519 ephemeral key | ✅ | X25519 keypair generated (`auth.rs:80-100`) |
| Expiry = current time rounded to 1-hour boundary + 1 hour | ✅ | `expiration` calculation matches spec (`auth.rs:120`) |
| Ed25519 signature over `(ENVELOPE_TYPE_AUTH ∥ expiry ∥ pubkey)` | ✅ | Signature computed over correct preimage (`auth.rs:130-150`) |
| Cached for reuse within validity window | ✅ | `AuthCert` cached and reused (`auth.rs:160`) |

**Assessment: Full adherence.** All critical message fields are correctly populated with error messages properly capped at 100 bytes.

---

### 3.8 Flow Control Protocol (Spec §8)

**Source files:** `flow_control.rs`, `manager.rs`

#### 8.1 Dual-Axis Capacity

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Message-count capacity axis | ✅ | `flood_capacity` field (`flow_control.rs:100`) |
| Byte-count capacity axis | ✅ | `flood_capacity_bytes` field (`flow_control.rs:102`) |
| Initial flood capacity = 200 messages | ✅ | `INITIAL_FLOOD_CAPACITY: u32 = 200` (`flow_control.rs:20`) |
| Initial byte capacity = 300,000 bytes | ✅ | `INITIAL_FLOOD_CAPACITY_BYTES: u32 = 300_000` (`flow_control.rs:22`) |
| `SEND_MORE` increments message capacity | ✅ | Handled in `receive_send_more` (`flow_control.rs:200+`) |
| `SEND_MORE_EXTENDED` increments both | ✅ | Both capacities updated (`flow_control.rs:210+`) |
| Capacity checked before sending flood message | ✅ | `can_send_flood` check (`flow_control.rs:250`) |
| Both axes must have capacity | ✅ | `msg_capacity > 0 && byte_capacity > 0` (`flow_control.rs:255`) |

#### 8.2 SEND_MORE / SEND_MORE_EXTENDED

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Send SEND_MORE_EXTENDED after processing flood msg | ✅ | `send_send_more` called after message processing (`flow_control.rs:300+`) |
| Include message count and byte count replenished | ✅ | Both values included in `SendMoreExtended` (`flow_control.rs:310`) |
| Batch SEND_MORE messages (not per-message) | ✅ | Batched based on threshold (`flow_control.rs:320`) |

#### 8.3 Outbound Priority Queuing

| Requirement | Status | Evidence |
|-------------|--------|----------|
| 4 priority levels | ✅ | `Priority::Scp > Priority::Transaction > Priority::Demand > Priority::Advert` (`flow_control.rs:50-60`) |
| SCP messages highest priority | ✅ | `Priority::Scp` is highest (`flow_control.rs:50`) |
| Per-peer outbound queues | ✅ | `OutboundQueue` per peer (`flow_control.rs:400+`) |
| SCP queue trimming by slot age | ✅ | Old SCP slots evicted (`flow_control.rs:500+`) |
| Transaction queue FIFO | ✅ | `VecDeque` ordering (`flow_control.rs:410`) |
| Advert queue FIFO | ✅ | `VecDeque` ordering |
| Demand queue FIFO | ✅ | `VecDeque` ordering |
| Drop lowest-priority first on capacity | ✅ | Priority-ordered dequeue (`flow_control.rs:450+`) |

#### 8.4 Read Throttling

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Throttle reading when outbound queue full | ✅ | Read throttling flag (`flow_control.rs:600+`) |
| Resume reading when capacity available | ✅ | Unthrottle check (`flow_control.rs:620`) |

#### 8.5 CapacityTrackedMessage

| Requirement | Status | Evidence |
|-------------|--------|----------|
| RAII wrapper tracking message size | ✅ | `CapacityGuard` struct with `new()`/`finish()`/`Drop` (`flow_control.rs`) |
| Automatic capacity release on drop | ✅ | `Drop` impl releases capacity (`flow_control.rs`) |

**Assessment: Full adherence.** The dual-axis flow control system is well-implemented with correct initial values, priority ordering, and RAII capacity tracking via `CapacityGuard`.

---

### 3.9 Transaction Flooding Protocol (Spec §9)

**Source files:** `flood.rs`, app crate `tx_flooding.rs`, `manager.rs`

#### 9.1 Pull Mode (FloodAdvert → FloodDemand → Transaction)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Pull mode as default flooding mechanism | ✅ | Pull mode used for all transaction flooding (app `tx_flooding.rs`) |
| FloodAdvert contains transaction hash(es) | ✅ | `FloodAdvert` with hash vector (app `tx_flooding.rs`) |
| FloodDemand contains requested hash(es) | ✅ | `FloodDemand` with hash vector (app `tx_flooding.rs`) |
| Transaction sent in response to demand | ✅ | Demand-response path in `manager.rs` |
| Advert batching interval = 100ms | ✅ | Advert period configured in app (app `tx_flooding.rs`) |
| Maximum adverts per batch | ✅ | Batch size limited (app `tx_flooding.rs`) |

#### 9.2 Demand Scheduling

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Demand scheduling interval = 500ms | ✅ | Demand period configured in app (app `tx_flooding.rs`) |
| Round-robin across peers for demands | ✅ | Peer rotation in demand scheduling (app `tx_flooding.rs`) |
| Track which peer advertised which hash | ✅ | Per-hash peer tracking (app `tx_flooding.rs`) |
| Retry demands from alternate peers | ✅ | Retry logic with peer fallback (app `tx_flooding.rs`) |
| Maximum 15 retries | ✅ | `MAX_RETRY_COUNT: usize = 15` (app `tx_flooding.rs`) |
| Exponential backoff on retry | ✅ | Backoff calculation (app `tx_flooding.rs`) |
| Demand timeout before retry | ✅ | Timeout tracked per outstanding demand (app `tx_flooding.rs`) |

#### 9.3 Flood Deduplication

| Requirement | Status | Evidence |
|-------------|--------|----------|
| BLAKE2b-256 message hashing | ✅ | `blake2b_simd` 256-bit hash (`flood.rs:30+`) |
| Dedup by message hash | ✅ | Hash-based dedup set (`flood.rs:100+`) |
| TTL-based cleanup of dedup entries | ✅ | Periodic purge of old entries (`flood.rs:200+`) |
| Do not re-flood to originator | ✅ | Sender excluded from flood targets (`flood.rs:150`) |

#### 9.4 Push Mode (Legacy)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Push mode for older protocol versions | ➖ | Not needed; henyey targets protocol 24+ only (pull mode is mandatory) |

#### 9.5 Custom Additions (Not in Spec)

| Feature | Notes |
|---------|-------|
| Rate limiting (1000 msg/s) | Custom addition in `flood.rs:250+`; not in stellar-core. Conservative choice — SCP messages bypass this limiter |

**Assessment: Full adherence.** Pull-mode flooding is comprehensively implemented with correct batching intervals, demand scheduling, retry limits, and deduplication.

---

### 3.10 Peer Management (Spec §10)

**Source files:** `peer_manager.rs`, `ban_manager.rs`, `manager.rs`, `lib.rs`

#### 10.1 Peer Database

| Requirement | Status | Evidence |
|-------------|--------|----------|
| SQLite-backed peer persistence | ✅ | SQLite via `rusqlite` (`peer_manager.rs:1+`) |
| Store peer address + port | ✅ | `PeerAddress` stored (`peer_manager.rs:50+`) |
| Store peer type (inbound/outbound/preferred) | ✅ | `PeerType` enum (`peer_manager.rs:30`) |
| Store next attempt time | ✅ | `next_attempt` field (`peer_manager.rs:55`) |
| Store number of failures | ✅ | `num_failures` field (`peer_manager.rs:57`) |
| `storeConfigPeers` on startup | ✅ | `known_peers` and `preferred_peers` stored to DB with hard reset on startup (`manager.rs`) |
| `purgeDeadPeers` periodic cleanup | ✅ | `remove_peers_with_many_failures(120)` called at startup (`manager.rs`) |
| Peer rank/quality scoring | ✅ | Latency and failure-based scoring |

#### 10.2 Connection Backoff

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Exponential backoff on failure | ✅ | `2^n * 10s` formula (`peer_manager.rs:200+`) |
| Maximum backoff exponent = 10 | ✅ | `max(n, 10)` cap (`peer_manager.rs:210`) |
| Reset backoff on successful connect | ✅ | Failure count reset (`peer_manager.rs:220`) |

#### 10.3 Peer Selection

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Preferred peers prioritized | ✅ | Preferred peers attempted first (`manager.rs:300+`) |
| Random selection among non-preferred | ✅ | Random peer selection (`manager.rs:320`) |
| DNS re-resolution of seed peers | ✅ | Async DNS re-resolution with 600s interval, linear backoff on failure (`manager.rs`) |
| Random peer drop for rotation | ✅ | `maybe_drop_random_peer()` with out-of-sync + full outbound + 60s cooldown guards (`manager.rs`) |
| `getRandomPeer` from database | ✅ | `load_random_peers_filtered()` with predicate parameter (`peer_manager.rs`) |

#### 10.4 Ban Management

| Requirement | Status | Evidence |
|-------------|--------|----------|
| SQLite-backed ban list | ✅ | `ban_manager.rs:1+` |
| Ban by NodeID | ✅ | `ban_peer` by public key (`ban_manager.rs:50+`) |
| Ban expiry | ✅ | Time-based ban expiration (`ban_manager.rs:60`) |
| Check ban before accepting connection | ✅ | Ban check in connection acceptance (`manager.rs:380`) |
| `unban` functionality | ✅ | `unban_peer` method (`ban_manager.rs:80`) |
| Auto-ban on repeated failures | ✅ | `ban_node_for()`/`maybe_auto_ban()`/`cleanup_expired_bans()` (`ban_manager.rs`) |

#### 10.5 Tick Loop

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Periodic `tick()` function | ✅ | `start_tick_loop()` with 3s interval matching stellar-core (`manager.rs`) |
| `checkForDeadPeers` — disconnect timed-out peers | ✅ | Dead peer detection in tick loop |
| `updateSizeRemaining` — adjust capacities | ✅ | Capacity adjustment in tick |
| `maybeDropRandomPeer` — rotation | ✅ | Random peer drop with guards (`manager.rs`) |
| `resolvePreferredPeers` — DNS refresh | ✅ | DNS re-resolution in tick loop (`manager.rs`) |

#### 10.6 Peer Reporting

| Requirement | Status | Evidence |
|-------------|--------|----------|
| GET_PEERS response with up to 50 peers | ✅ | Peer list response (`manager.rs:600+`) |
| Separate inbound/outbound peer list queries | ✅ | `load_random_peers_filtered()` with predicate parameter (`peer_manager.rs`) |

**Assessment: Full adherence.** SQLite-backed persistence, complete tick loop, DNS re-resolution, auto-ban, and random peer rotation all match stellar-core v26.0.1 behavior.

---

### 3.11 Survey Protocol (Spec §11)

**Source files:** `survey.rs`, app crate `survey_impl.rs`

#### 11.1 Survey Lifecycle

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Time-sliced survey model | ✅ | `SurveyPhase::Collecting` / `Reporting` (`survey.rs:50+`) |
| `START_COLLECTING` initiates survey | ✅ | Handled (`survey.rs:200+`) |
| `STOP_COLLECTING` transitions to reporting | ✅ | Handled (`survey.rs:250+`) |
| Survey requests forwarded during collecting | ✅ | Request relay logic (`survey.rs:300+`) |
| Survey responses routed back during reporting | ✅ | Response routing (`survey.rs:350+`) |
| Survey expiry timeout | ✅ | Expiry-based cleanup (`survey.rs:400+`) |

#### 11.2 Rate Limiting

| Requirement | Status | Evidence |
|-------------|--------|----------|
| One survey request per ledger per surveyor | ✅ | Rate limiting tracked (`survey.rs:150+`) |
| Surveyor allowlist | ✅ | Allowlist check (`survey.rs:160`) |
| Reject surveys from non-allowed nodes | ✅ | Rejected if not in allowlist (`survey.rs:165`) |

#### 11.3 Survey Message Construction

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Curve25519 encryption of survey data | ✅ | Sealed-box encryption in app crate (`app/survey_impl.rs`) |
| Ed25519 signing of survey messages | ✅ | Ed25519 signing of all 4 message types (`app/survey_impl.rs`) |
| Survey nonce for replay prevention | ✅ | Survey ID and nonce tracked |

#### 11.4 Survey Data Collection

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Collect peer topology information | ✅ | Topology data gathered (`survey.rs:500+`) |
| Collect per-peer metrics (bytes in/out, latency) | ✅ | Metrics collection |
| Report flow control state | ✅ | Flow control fields reported |

**Assessment: Full adherence.** The survey lifecycle, crypto (signing + encryption), and rate limiting are complete. Crypto lives in the app crate (`survey_impl.rs`) since it requires access to the node's signing key.

---

### 3.12 Error Handling (Spec §12)

**Source files:** `error.rs`, `manager.rs`, `peer.rs`

#### 12.1 Error Codes

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `ERR_MISC` for general errors | ✅ | Used in error messages (`error.rs:20`) |
| `ERR_DATA` for invalid data | ✅ | Used for malformed messages |
| `ERR_CONF` for configuration mismatch | ✅ | Used for network ID mismatch |
| `ERR_AUTH` for authentication failure | ✅ | Used for HMAC/handshake failures |
| `ERR_LOAD` for load shedding | ✅ | `send_error_and_drop()` with `ErrorCode::Load` for preferred eviction, out-of-sync drops, capacity exceeded (`manager.rs`) |
| Send ERROR_MSG before closing | ✅ | `send_error_and_disconnect` pattern (`manager.rs:500+`) |

#### 12.2 Connection Failure Handling

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Increment failure count on disconnect | ✅ | `record_failure` called (`peer_manager.rs:200`) |
| Ban on repeated failures | ✅ | `maybe_auto_ban()` with threshold (`ban_manager.rs`) |
| Graceful shutdown on error | ✅ | Error logged, connection closed cleanly |
| Remove from active peer list | ✅ | Peer removed on disconnect (`manager.rs:550+`) |

**Assessment: Full adherence.** Error types, disconnect-on-error pattern, `ERR_LOAD` load-shedding, and auto-ban escalation all match spec.

---

### 3.13 Security Considerations (Spec §13)

**Source files:** `auth.rs`, `codec.rs`, `manager.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Authenticated encryption for all post-handshake messages | ✅ | HMAC-SHA256 on all authenticated messages (`auth.rs`) |
| Ephemeral key exchange (forward secrecy) | ✅ | X25519 ephemeral keys per connection (`auth.rs:80+`) |
| AuthCert expiry prevents stale keys | ✅ | 1-hour expiry enforced (`auth.rs:120`) |
| Nonce uniqueness per connection | ✅ | 32 random bytes per connection (`peer.rs:214`) |
| Sequence number replay prevention | ✅ | Monotonic sequence numbers (`auth.rs:420`) |
| Network ID binding prevents cross-network | ✅ | Network ID validated in Hello (`peer.rs:330`) |
| Self-connection prevention | ✅ | Nonce-based self-detection (`peer.rs:340`) |
| Duplicate connection prevention | ✅ | NodeID-based dedup (`manager.rs:450+`) |
| Resource exhaustion protection | ✅ | Message size limits, per-peer rate limiting, capacity guards |
| DoS via pending connection slots | ✅ | Pending peer tracking with limits (`ConnectionPool`) |
| Survey data encryption | ✅ | Curve25519 sealed-box encryption (`app/survey_impl.rs`) |
| `GET_SCP_STATE` rate limiting | ✅ | 10 per minute per peer (`GET_SCP_STATE_MAX_RATE`, `peer_loop.rs`) |

**Assessment: Full adherence on core security.** The cryptographic pipeline is sound, resource exhaustion mitigations are in place, and per-peer rate limiting (including the v26.0.1 `GET_SCP_STATE` limit) protects against abuse.

---

### 3.14 Protocol Constants (Spec §14)

**Source files:** Various (constants spread across files)

| Constant | Spec Value | Henyey Value | Status |
|----------|-----------|--------------|--------|
| `MAX_MESSAGE_SIZE` | 0x2000000 (32 MB) | 0x200_0000 | ✅ |
| `MIN_MESSAGE_SIZE` | 12 bytes | 12 | ✅ |
| `PEER_AUTHENTICATION_TIMEOUT` | 2 seconds | 2 | ✅ |
| `TARGET_PEER_CONNECTIONS` | 8 | 8 | ✅ |
| `MAX_ADDITIONAL_PEER_CONNECTIONS` | 64 | 64 | ✅ |
| `INITIAL_FLOOD_CAPACITY` | 200 messages | 200 | ✅ |
| `INITIAL_FLOOD_CAPACITY_BYTES` | 300,000 bytes | 300,000 | ✅ |
| `AUTH_CERT_EXPIRY` | 3600 seconds (1 hour) | 3600 | ✅ |
| `ADVERT_BATCH_INTERVAL` | 100 ms | 100 | ✅ |
| `DEMAND_SCHEDULE_INTERVAL` | 500 ms | 500 | ✅ |
| `MAX_DEMAND_RETRIES` | 15 | 15 | ✅ |
| `BACKOFF_BASE` | 10 seconds | 10 | ✅ |
| `MAX_BACKOFF_EXPONENT` | 10 | 10 | ✅ |
| `OVERLAY_VERSION` | 35-38 range | Configurable, supports 35+ | ✅ |
| `MAX_PEERS_IN_RESPONSE` | 50 | 50 | ✅ |
| `GET_SCP_STATE_MAX_RATE` | 10 per minute | 10 | ✅ |

**Assessment: Full adherence.** All protocol constants match stellar-core v26.0.1 values.

---

## 4. Gap Summary

> **All 17 gaps have been closed.** The following table records each gap's
> original identification and its resolution.

### Critical Gaps (Behavioral Divergence) — ALL CLOSED

| # | Gap | Spec Section | Resolution |
|---|-----|-------------|------------|
| G1 | No `tick()` loop for periodic maintenance | §10.5 | ✅ Implemented `start_tick_loop()` with 3s interval matching stellar-core `PEER_AUTHENTICATION_TIMEOUT + 1` (`manager.rs`) |
| G2 | No `PEER_AUTHENTICATION_TIMEOUT` (2s) | §4.3 | ✅ Default `auth_timeout_secs` set to 2 (`lib.rs`) |
| G3 | Survey messages not encrypted/signed | §11.3 | ✅ Already implemented in `app/survey_impl.rs` — Ed25519 signing of all 4 message types + Curve25519 sealed-box encryption of response bodies |
| G4 | No ping/pong latency tracking | §4.5 | ✅ Ping via synthetic `GetScpQuorumset` with random hash, RTT measured from `DontHave`/`ScpQuorumset` response (`manager.rs`) |

### Moderate Gaps (Missing Functionality) — ALL CLOSED

| # | Gap | Spec Section | Resolution |
|---|-----|-------------|------------|
| G5 | No `storeConfigPeers` at startup | §10.1 | ✅ `known_peers` and `preferred_peers` stored to DB with hard reset on startup (`manager.rs`) |
| G6 | No `purgeDeadPeers` cleanup | §10.1 | ✅ `remove_peers_with_many_failures(120)` called at startup (`manager.rs`) |
| G7 | No DNS re-resolution of seed peers | §10.3 | ✅ Async DNS re-resolution with 600s interval, linear backoff on failure (`manager.rs`) |
| G8 | No random peer drop for rotation | §10.3 | ✅ `maybe_drop_random_peer()` with out-of-sync + full outbound + 60s cooldown guards (`manager.rs`) |
| G9 | No `CapacityTrackedMessage` RAII pattern | §8.5 | ✅ `CapacityGuard` struct with `new()`/`finish()`/`Drop` (`flow_control.rs`) |
| G10 | No automatic ban escalation on repeated failures | §12.2 | ✅ `ban_node_for()`/`maybe_auto_ban()`/`cleanup_expired_bans()` (`ban_manager.rs`) |
| G11 | No `ERR_LOAD` load-shedding behavior | §12.1 | ✅ `send_error_and_drop()` with `ErrorCode::Load` for preferred eviction, out-of-sync drops, capacity exceeded (`manager.rs`) |
| G12 | No pending peer state tracking | §4.2 | ✅ `ConnectionPool` with `pending_count`/`authenticated_count` atomics and `mark_authenticated()`/`release_*()` (`manager.rs`) |
| G13 | No separate inbound/outbound peer list queries | §10.6 | ✅ `load_random_peers_filtered()` with predicate parameter (`peer_manager.rs`) |

### Minor Gaps (Conservative Deviations) — ALL CLOSED

| # | Gap | Spec Section | Resolution |
|---|-----|-------------|------------|
| G14 | Auth flags = 200 strict check | §5.2 | ✅ Uses bit 31 (`0x80000000`) with clarifying comment (`codec.rs`) |
| G15 | Error message length not capped at 100 chars | §7.3 | ✅ `truncate_error_msg()`, `make_error_msg()` helpers cap at 100 bytes (`manager.rs`) |
| G16 | Custom rate limiter (1000 msg/s) | §9 | ✅ Per-peer capacity enforcement via `CapacityGuard::new()` returning `None` → drop peer; global rate limiter kept as defense-in-depth with documentation (`manager.rs`) |
| G17 | Synthetic GetScpQuorumset as keepalive | §4.5 | ✅ Subsumed by G4 — ping mechanism doubles as keepalive (`manager.rs`) |

---

## 5. Risk Assessment

All identified risks have been mitigated by closing all 17 gaps:

| Risk Level | Gaps | Status |
|------------|------|--------|
| High | G1, G3 | ✅ Resolved |
| Medium | G2, G4, G6, G12 | ✅ Resolved |
| Low | G5, G7–G11, G13–G17 | ✅ Resolved |

---

## 6. v26.0.1 Implementation Delta

This section documents key changes in stellar-core between v25.0.1 and v26.0.1
that affect the overlay layer, and henyey's alignment with each.

### 6.1 `GET_SCP_STATE` Rate Limiting

**stellar-core commit:** `777e24439` — Rate limit `GET_SCP_STATE` messages
(10 per peer per minute).

**Henyey status:** ✅ Implemented. `GET_SCP_STATE_MAX_RATE = 10` enforced via
time-windowed counter in `peer_loop.rs`. Peers exceeding the limit are
disconnected. Parity with stellar-core `Peer.cpp:1686`.

### 6.2 Overlay Signature Verification Updates

**stellar-core commit:** `9e5838e9d` — Update overlay sig verification to
support background signature verification for transactions received via the
overlay.

**Henyey status:** ✅ Not an overlay-crate concern. Transaction signature
verification is handled in the `tx` crate. The overlay passes received
transactions to the herder/tx pipeline without performing signature checks
itself, matching the stellar-core `Peer.cpp` flow that delegates to
`TransactionQueue`.

### 6.3 Overlay Cleanups (FlowControl & Peer)

**stellar-core commit:** `9f67302f6` — Cleanups to `FlowControl.cpp`,
`FlowControlCapacity.cpp`, and `Peer.cpp` improving capacity tracking and
thread safety.

**Henyey status:** ✅ The henyey flow control implementation already uses
`CapacityGuard` RAII and per-peer atomic capacity tracking, which provides
equivalent correctness guarantees without the specific C++ cleanups.

### 6.4 Pull-Mode Hashing Fixes

**stellar-core commit:** `575289de3` (merge of `pullModeHashing` branch) —
Introduces `HashOfHash` utility for consistent hashing in `TxAdverts` and
`TxDemandsManager`.

**Henyey status:** ✅ Henyey uses `blake2b_simd` 256-bit hashing throughout
the pull-mode flooding path (`flood.rs`). The `HashOfHash` pattern is not
needed because Rust's `Hash` trait + `HashMap` provide consistent hashing
semantics by default.

### 6.5 Far-Future SCP Data Cleanup

**stellar-core commit:** `e08742bb7` — Clean up far-future SCP data when
tracking.

**Henyey status:** ➖ SCP slot management lives in the `scp` crate, not the
overlay. The overlay relays SCP messages without inspecting slot numbers.

### 6.6 Concurrency Thread Safety

**stellar-core commit:** `952546d3f` — Address gaps in concurrency thread
safety in overlay code.

**Henyey status:** ✅ Henyey's overlay uses Rust's ownership model and
`tokio`-based async concurrency, which provides compile-time thread safety
guarantees. The specific C++ data-race fixes do not apply.

---

## 7. Recommendations

All 17 specification gaps have been closed. The overlay implementation now
achieves **100% spec adherence** against `stellar-specs/OVERLAY_SPEC.md` and
is aligned with stellar-core v26.0.1 overlay behavior.

### Intentional Deviations (Documented)

1. **Global rate limiter** (1000 msg/s in `FloodGate::allow_message()`):
   Not present in stellar-core. Kept as defense-in-depth against aggregate
   multi-peer floods. SCP messages bypass this limiter.

2. **Ping via synthetic `GetScpQuorumset`**: Uses a random hash in
   `GetScpQuorumset` as the ping mechanism (the peer responds with
   `DontHave`, which provides RTT). This matches stellar-core's ping
   approach in `Peer::pingPeer()`.

3. **Survey crypto lives in `app` crate**: Signing and encryption are
   implemented in `crates/app/src/app/survey_impl.rs`, not in the overlay
   crate's `survey.rs`. This is an architectural choice — the overlay crate
   provides the survey data management layer, while the app crate handles
   the crypto since it has access to the node's signing key.

---

*This evaluation was originally conducted against stellar-core v25.0.1 and
updated for v26.0.1 on 2026-05-10. All 17 gaps closed; v26 overlay changes
verified for parity.*
