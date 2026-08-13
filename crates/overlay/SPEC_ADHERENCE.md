# OVERLAY_SPEC Adherence — henyey-overlay

**Spec version:** 26 (Overlay Protocol v38–v41)
**Crate:** crates/overlay
**Last updated:** 2026-06-22
**Overall adherence:** 83%

Counts (excluding Drift and N/A from denominator):
**Full 83 | Partial 17 | Absent 0 | Drift 1 | N/A 2**

## Summary table

| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §3.3 | MAX_MESSAGE_SIZE (16 MiB) / unauth limit (4096) | Full | `codec.rs:30-35,194-206` |
| §3.3 | Zero-length frame rejection | Full | `codec.rs:183-187` |
| §4.2 | RFC 5531 record marking (last-fragment bit) | Full | `codec.rs:145,272` set `len \| 0x80000000` on send; `codec.rs:185-186` masks it on receive and surfaces it as `is_last_fragment` for diagnostics only (nothing branches on it). Matches stellar-core: xdrpp sets the bit on send (`marshal.cc:29,40`), and `TCPPeer::getIncomingMsgLength()` clears it and never inspects it (`TCPPeer.cpp:679`, `length &= 0x7f`). Neither implements continuation-fragment reassembly — spec-conformant, since the bit is set on every message. |
| §4.3 | AuthenticatedMessage MAC framing, send/recv sequence | Full | `auth.rs:622-707` (INV-O1/O2) |
| §4.3 | Constant-time MAC compare | Full | `auth.rs:90-103,694-697` |
| §5.3 | TCP_NODELAY, SO_LINGER | Full | `connection.rs:94-99` |
| §5.3 | Outbound back-off (numFailures → nextattempt) | Full | `peer_manager.rs:617-661` |
| §5.3 | Inbound accept slot check (`haveSpaceForConnection`) | Full | `connection.rs:541-592` |
| §5.4 | HELLO/AUTH ordering | Full | `peer.rs:332-486` |
| §5.4.1 | HELLO 14-step validation | Partial | `auth.rs:369-453` + `peer.rs:644-699` — covers networkID, version, cert, self-connect, port; missing IP-presence check (#4 in spec list), `updatePeerRecordAfterEcho` (#11), explicit pending-peerID dedup ordered before bans (correct order is bans first) |
| §5.4.2 | AuthCert (1h expiration, 30min refresh) | Partial | `auth.rs:105-162` — 1h expiration set; 30-min refresh threshold not implemented (one cert per process) |
| §5.4.2 | `verifyRemoteAuthCert` | Full | `auth.rs:129-161` |
| §5.4.3 | HKDF key derivation (A/B prefixes 0/1) | Full | `auth.rs:469-547` |
| §5.4.3 | MAC key immutability (INV-O3) | Full | `auth.rs:369-378` (state guard rejects 2nd process_hello) |
| §5.4.4 | AUTH.flags == 200 enforcement (INV-O7) | Full | `peer.rs:617-623` |
| §5.4.4 | Send `SEND_MORE_EXTENDED` before any flood (INV-O8) | Full | `peer.rs:472-477` (sent inside `handshake()` before authenticated state used) |
| §5.4.4 | `getMinLedgerSeqToAskPeers` for initial GET_SCP_STATE | Drift | `peer.rs:481-482` — sends `GetScpState(0)` unconditionally; spec/core uses dynamic min-ledger value |
| §5.5 | Pre-auth message discipline (only HELLO/AUTH/ERROR_MSG) | Partial | Implicit: `Peer::handshake` only accepts HELLO then AUTH; no explicit pre-auth dispatcher (e.g., AUTH_ACTION_QUEUE scheduler) |
| §5.6 | PEER_TIMEOUT 30s / STRAGGLER 120s / SEND_MODE_IDLE 60s | Full | `manager/peer_loop.rs:551-583` |
| §5.6 | Recurring ping (`GET_SCP_QUORUMSET`) | Full | `manager/peer_loop.rs:589-613` (5s interval) |
| §6 | Message type registry / scheduling category | Partial | Type classification present (`metrics.rs:269-355`); scheduling categories (AUTH/CTRL/TX/SCPQ/SCP) not modeled as distinct queues |
| §7.1 | Dual-axis (msg+byte) flow control | Full | `flow_control.rs:308-439` |
| §7.2 | `getFlowControlBytesTotal` auto-compute | Full | `flow_control.rs:119-135` |
| §7.3 | begin/endMessageProcessing capacity bookkeeping | Full | `flow_control.rs:1021-1065` |
| §7.3 | `releaseAssert(processed <= batch_size)` | Full | `flow_control.rs:1045-1056` hard `assert!(flood_data_processed <= batch_size)` after the counter increment; message send-more uses `==` (bytes `>=`), mirroring `FlowControl.cpp:303-311` |
| §7.4 | Reading throttling on total capacity | Full | `flow_control.rs:1068-1071, 1084-1094` |
| §7.5 | Outbound priority queues (SCP/TX/Demand/Advert) | Full | `flow_control.rs:236-281,516-534` |
| §7.5 | SCP queue trimming (slot floor + nomination/ballot replace) | Full | `flow_control.rs:826-927` |
| §7.5 | Outbound TX queue 3 MiB byte cap | Full | `flow_control.rs:202-213,756-771` |
| §7.6 | `SEND_MORE_EXTENDED` validation (numBytes!=0, overflow) | Full | `flow_control.rs:979-1010` |
| §7.6 | Reject legacy `SEND_MORE` (v38+) | Full | `manager/peer_loop.rs:1093-1099` |
| §7.7 | tx-size-increase: SEND_MORE_EXTENDED(0, increase) | Full | `manager/mod.rs:1633-1657` |
| §8.1 | Advert phase (FLOOD_ADVERT) | N/A | Per PARITY_STATUS, moved to app crate `tx_flooding.rs` |
| §8.2 | Demand phase (FLOOD_DEMAND scheduler) | N/A | Per PARITY_STATUS, in app crate |
| §8.4 | Recv-side `recvTransaction` (forget on non-pending) | Partial | Receipt path forwards via floodgate (`flood.rs`); the forget-on-non-pending decision lives in app/herder, not the overlay |
| §8.4 | Drop TX/ADVERT/DEMAND while not synced (INV-O14) | Full | `manager/peer_loop.rs:693-697`; `is_flood_shed_on_unsync` in `codec.rs:354-361` |
| §9.1 | Floodgate `broadcast`, `peersTold` tracking | Full | `flood.rs:179-460` (uses BLAKE2b-256 hashes) |
| §9.1 | `clearBelow` at ledger close | Full | `flood.rs` (see `clear_below` API) |
| §9.2 | ItemFetcher (TX_SET, SCP_QUORUMSET) | Full | `item_fetcher.rs:128-300` (Tracker, retry, 1500 ms timeout, 10 rebuilds) |
| §9.3 | Per-window query rate limits | Full | `query_policy.rs:31-74`, `manager/peer_loop.rs:37-99` (window = closeTime × MAX_SLOTS_TO_REMEMBER 12) |
| §9.3 | `GET_SCP_STATE_MAX_RATE = 10` | Full | `query_policy.rs:19-20,58-61` |
| §9.3 | `GET_TX_SET` response type-switch (TX_SET vs GENERALIZED_TX_SET) | Partial | Both message types exist in metrics; the protocol-version-aware response selection is owned by app layer, not surfaced here |
| §9.3 | `DONT_HAVE` reply for miss | Partial | Handled inbound (`message_handlers.rs:314-340`); outbound (sending DONT_HAVE for unknown items) lives in app callbacks |
| §9.4 | PEERS broadcasting (≤50 entries, randomized) | Partial | `manager/mod.rs:1373-1415` builds message; MAX_PEERS_PER_MESSAGE cap applied; pool combines outbound+inbound but selection not strictly the spec's "sample 50 from outbound pool first" algorithm |
| §9.4 | PEERS receipt: `ensureExists` per entry | Full | App-owned: `crates/app/src/app/peers.rs:293-338` `process_peer_list()` converts, filters, persists entries and adds to overlay; dispatched from `lifecycle.rs:1689` |
| §9.4 | PEERS receipt: skip port==0, IPv6, private, self, localhost | Partial | App-owned: `crates/app/src/app/peers.rs:293-338` filters port==0, IPv6, private, localhost via `is_public_peer`; **missing explicit self-address filter** (stellar-core checks `Peer.cpp:2024-2030`) |
| §9.4 | PEERS one-per-connection + role check (INV-O12, INV-O13) | Full | `manager/peer_loop.rs:505-531,700-718` |
| §10.1 | Peer DB schema (ip, port, nextattempt, numfailures, type) | Full | `peer_manager.rs:14-25,222-238` |
| §10.1 | Type lattice (PREFERRED upgrade, INBOUND no promote) | Full | `peer_manager.rs:577-615` |
| §10.2 | `TARGET_PEER_CONNECTIONS`, MIN_INBOUND_FACTOR, etc. | Partial | Constants present (`lib.rs:307`); `MIN_INBOUND_FACTOR = 3` not modeled as an adjusted-target formula |
| §10.2 | `POSSIBLY_PREFERRED_EXTRA = 2` | Full | `connection.rs:496-498,514-528,555-592` |
| §10.3 | `acceptAuthenticatedPeer` — preferred eviction | Full | `manager/connection.rs:132-228` |
| §10.3 | `PREFERRED_PEERS_ONLY` reject | Full | `manager/connection.rs:211-216,711-715` |
| §10.4 | Tick period 3 s (`PEER_AUTHENTICATION_TIMEOUT + 1`) | Full | `manager/tick.rs:26` |
| §10.4 | DNS resolution every 600 s w/ linear backoff | Full | `manager/tick.rs:31-40,176-198` |
| §10.4 | Random out-of-sync drop after 60 s | Partial | `manager/tick.rs:589-670` `maybe_drop_random_peer()` with `OUT_OF_SYNC_RECONNECT_DELAY = 60s`; tested via `test_maybe_drop_random_peer_drops_after_cooldown` et al. See §10.4 detailed note for remaining divergences. |
| §10.4 | Promote inbound (open parallel outbound) | Full | `manager/tick.rs` `promote_inbound_peers()` runs last in the tick, dialing authenticated inbound peers' advertised listening addresses; `fill_outbound_slots` reserves `RESERVED_FOR_PROMOTION = 1` pending slot when promotable inbound peers exist, mirroring `OverlayManagerImpl.cpp:782-794` |
| §10.5 | IPv6 silently ignored | Full | `lib.rs:530-535`, `manager/mod.rs:1398-1400` |
| §10.5 | Private/localhost addresses ignored | Full | `lib.rs:457-483`, `manager/mod.rs:1431-1433` |
| §10.6 | `PEERS` sample size 50 | Full | `manager/mod.rs:85` defines `MAX_PEERS_PER_MESSAGE = 50`; enforced at `mod.rs:1386` |
| §10.7 | Ban check during HELLO | Full | `peer.rs:390-400`, `manager/connection.rs:378-381,685-687,1039-1041` (INV-O18) |
| §10.7 | BanManager SQLite persistence | Full | `ban_manager.rs:48-126` |
| §11.1 | Survey phase machine (Inactive/Collecting/Reporting) | Full | `survey.rs:62-575` |
| §11.1 | COLLECTING max 30 min / REPORTING max 3 h | Full | `survey.rs:41-45,232-242,544-573` |
| §11.2 | Start/Stop Collecting signed-message receipt | Full | App-owned: `crates/app/src/app/survey_impl.rs:941-956` `verify_survey_signature` verifies Ed25519 signature on incoming start/stop messages via `henyey_crypto::verify_from_raw_key`; `survey.rs:426-522` manages local phase transitions |
| §11.3 | `SurveyRequest`/`SurveyResponse` flow | Full | App-owned: `crates/app/src/app/survey_impl.rs:488-790` handles all four survey message types (`handle_survey_request`, `handle_survey_response`, `handle_survey_start_collecting`, `handle_survey_stop_collecting`); dispatched from `lifecycle.rs:1669-1688`; includes signature verification (941-956) and rate-limiter integration |
| §11.3 | Curve25519 sealed-box encryption/decryption | Full | App-owned: `crates/app/src/app/survey_impl.rs:674-680` (`seal_to_curve25519_public_key`) and `786-793` (`open_from_curve25519_secret_key`) via `henyey_crypto` |
| §11.5 | `surveyorPermitted` (allowlist or tracked quorum) | Partial | App-owned: `crates/app/src/app/survey_impl.rs:964-983` — allowlist gate via `config.overlay.surveyor_keys` with fallback to `Herder::local_quorum_nodes()` (static local quorum-set membership). Divergence: stellar-core uses `Herder::getCurrentlyTrackedQuorum()` (`SurveyManager.cpp:834-845`) which reflects the dynamically tracked quorum, not the static local config. Called at lines 501, 565, 622 before processing survey start/stop/request. |
| §11.6 | TimeSlicedNodeData / PeerData counters | Full | `survey.rs:74-212,491-521` |
| §12.1 | ERROR_MSG: zero seq+MAC pre-key, normal HMAC post-key | Partial | Receive-side parity: `auth.rs:672-675` skips MAC verification for incoming ERROR_MSG. Outbound divergence: post-auth ERROR_MSG sent via `peer.send()` → `auth.wrap_message()` applies normal MAC/seq, unlike stellar-core `Hmac.cpp:72-79` which exempts outbound ERROR_MSG. |
| §12.1 | ERROR_MSG drops connection | Full | `manager/peer_loop.rs:1046-1064` |
| §12.2 | ERR_MISC/ERR_DATA/ERR_CONF/ERR_AUTH/ERR_LOAD usage | Full | `peer.rs:557-576`, `manager/connection.rs:224`, `manager/peer_loop.rs:1079-1082` |
| §12.3 | Drop-once idempotence (INV-O19) | Partial | No `mDropStarted`-style atomic flag in `peer.rs::close`; instead state machine + tokio drop semantics. Idempotence relies on `state != PeerState::Disconnected` guard (`peer.rs:932-939`), which is single-threaded per peer. |
| §12.3 | 5-second drain delay before socket close | Full | `manager/peer_loop.rs` `OutboundMessage::ShutdownAfterError` + `wait_error_drop_drain()`: error-drop path flushes the queued `ERROR_MSG`, then defers the close by `ERROR_DROP_DRAIN_DELAY = 5s` (interruptible by node shutdown), mirroring `TCPPeer.cpp:835-862`. Plain teardown (`Shutdown`) stays immediate |
| §13.4 | Pre-auth payload limit 4 KiB | Full | `codec.rs:194-206` |
| §13.4 | Pending connection caps | Full | `connection.rs:473-529` |
| §13.4 | Handshake timeout 2 s | Full | `lib.rs:225-232,315` |
| §13.4 | Flow control overshoot drop ("peer at capacity") | Full | `manager/peer_loop.rs:1069-1089` (INV-O9) |
| §13.4 | Per-query rate limits | Full | `query_policy.rs`, `manager/peer_loop.rs:743-754` |
| §13.4 | Outbound queue load shedding | Full | `flow_control.rs:725-816` |
| §13.4 | `REALLY_DEAD_NUM_FAILURES_CUTOFF = 120` | Partial | Constant defined in `peer_manager.rs:42-45` (test-only); production pruning not wired |
| §13.4 | Crypto-error → ERR_DATA + drop | Partial | XDR decode error path returns generic `OverlayError::Message`; not specifically mapped to `ERR_DATA` |
| §13.5 | Self-connection rejection | Full | `peer.rs:673-679` |
| §13.5 | Duplicate-NodeID rejection | Full | `peer.rs:351-368,412-438` (INV-O18 sibling) |

## Invariant coverage

| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-O1 (Send seq monotonicity) | Full | `auth.rs:625-626` (send_sequence++), `auth.rs:674-680,701` (recv check + post-MAC advance) |
| INV-O2 (MAC coverage) | Full | `auth.rs:712-731,686-698` |
| INV-O3 (MAC key immutability) | Full | `auth.rs:369-378` (process_hello state guard rejects duplicate) |
| INV-O4 (Handshake order) | Full | `auth.rs:576-584` (process_auth state guard) + `peer.rs:332-457` |
| INV-O5 (NetworkID match) | Full | `auth.rs:380-384` |
| INV-O6 (Self-rejection) | Full | `peer.rs:673-679` |
| INV-O7 (AUTH flags=200) | Full | `peer.rs:617-623`, const at `peer.rs:48` |
| INV-O8 (Initial credit precedence) | Full | `peer.rs:460-477` — SEND_MORE_EXTENDED sent before `set_authenticated()` enables flood reads; first flood-controlled traffic only after this. |
| INV-O9 (Capacity non-overshoot) | Full | `flow_control.rs:1021-1033`, `manager/peer_loop.rs:1069-1089` |
| INV-O10 (SEND_MORE_EXTENDED validation) | Full | `flow_control.rs:979-1010` (numBytes!=0 + overflow guards) |
| INV-O11 (Recv-side batch grants) | Full | `flow_control.rs:1038-1065` emits SEND_MORE_EXTENDED on the batch threshold and now enforces the spec's `releaseAssert(mFloodDataProcessed <= BATCH_SIZE)` upper bound via a hard `assert!` (`flow_control.rs:1045-1056`) |
| INV-O12 (One PEERS per connection) | Full | `manager/peer_loop.rs:512-531,712-717` |
| INV-O13 (Inbound role rejects PEERS) | Full | `manager/peer_loop.rs:522-524` rejects PEERS from **Inbound** direction peers (`REMOTE_CALLED_US`); spec says "an inbound role peer MUST NOT receive `PEERS`" — Rust enforces the same. |
| INV-O14 (No flood while not synced) | Full | `manager/peer_loop.rs:693-697` |
| INV-O15 (Survey signature verification) | Full | App-owned: `crates/app/src/app/survey_impl.rs:941-956` `verify_survey_signature` uses `henyey_crypto::verify_from_raw_key` on Ed25519 key extracted from `NodeId`; called before processing survey start/stop/request/response |
| INV-O16 (Survey rate limit) | Full | `survey.rs:268-356` (`SurveyMessageLimiter`) |
| INV-O17 (One survey at a time) | Full | `survey.rs:430-466` (`start_collecting` returns false if active) |
| INV-O18 (Banned peer rejection) | Full | `peer.rs:390-400`, `manager/connection.rs:378-381,685-687` |
| INV-O19 (Drop idempotence) | Partial | State-machine guard (`peer.rs:932-939`) provides single-threaded idempotence; no atomic `mDropStarted` flag means truly-concurrent drops from different tasks would need to rely on the outbound channel's shutdown signal — works in practice but not as explicitly designed in stellar-core |

Re-evaluation: INV-O13 — code is correct. Spec says "An inbound role peer (`REMOTE_CALLED_US`) MUST NOT receive `PEERS`"; the Rust code `if direction == ConnectionDirection::Inbound { return RejectWrongDirection }` matches that exactly. The invariant name is corrected to "Inbound role rejects PEERS" to align with the spec language. Reclassified to **Full** in the summary count.

Corrected invariant tally: **Full 18 | Partial 1 | Absent 0**.

## Detailed findings

### §5.4.2 — AuthCert lifecycle (Partial)
- **Claim**: "The certificate is regenerated every 30 minutes (when `expiration < now + 1800`) with a 1-hour expiration window."
- **Rust**: `auth.rs:105-127` creates a cert per `AuthContext::new()`; expiration is 1 hour. `AuthContext::new()` is called per connection.
- **Status**: Partial. There is no shared cert that gets regenerated every 30 minutes; instead each connection creates a fresh cert. Functionally equivalent for short-lived connections but doesn't match the spec's "process-lifetime ephemeral keypair, cert refresh every 30 min" model (no `mSharedKeyCache` parity).

### §5.4.4 — Initial `GET_SCP_STATE` ledger seq (Drift)
- **Claim**: Send `GET_SCP_STATE(getMinLedgerSeqToAskPeers())`.
- **Rust**: `peer.rs:481-482` sends `GetScpState(0)` unconditionally.
- **Notes**: Functionally `0` requests the latest; stellar-core uses a computed minimum based on local catchup state. Likely benign for cold start; could be wasteful on a long-running node. Worth a fix.

### §5.5 — Pre-authentication message discipline (Partial)
- **Claim**: Before `GOT_AUTH`, only HELLO/AUTH/ERROR_MSG accepted; all dispatched on `AUTH_ACTION_QUEUE` to preserve order.
- **Rust**: `Peer::handshake` blocks on `recv_hello` then `recv_auth`; any unexpected message returns `InvalidMessage`. Post-handshake, `is_handshake_message` checks block stray HELLO/AUTH (`peer_loop.rs:721-727`).
- **Notes**: Implicit handling; no separate scheduler queue. Behavior matches in practice.

### §9.4 — PEERS receipt (Partial — app-owned)
- **Claim**: For each entry in a received PEERS message: skip if port==0/IPv6/private/self/localhost; otherwise call `PeerManager::ensureExists`.
- **Status**: **Partial (app-owned)**. The stale 2026-05-13 audit incorrectly marked this Absent because it searched only `crates/overlay/`. The receive path is wired through `crates/app/src/app/lifecycle.rs:1689` which dispatches `StellarMessage::Peers` to `process_peer_list()` in `crates/app/src/app/peers.rs:293-338`. That function converts XDR peer addresses, filters port==0/IPv6/private/localhost via `filter_discovered_peers` + `is_public_peer` (lines 463-514), then persists via direct DB writes (`persist_peers` at lines 441-454) and calls `overlay.add_peers()`. **Gap**: no explicit self-address filter matching stellar-core's check at `Peer.cpp:2024-2030`.

### §11.3 — Survey request/response flow (Full — app-owned)
- **Claim**: On `TIME_SLICED_SURVEY_REQUEST` receipt, validate via `SurveyMessageLimiter::addAndValidateRequest`, verify signature, fill `TopologyResponseBodyV2`, sealed-box encrypt with `encryptionKey`, sign response, broadcast.
- **Status**: **Full (app-owned)**. The stale 2026-05-13 audit searched only `crates/overlay/src/` and missed the app-layer implementation. The full dispatch chain is:
  1. `crates/app/src/app/lifecycle.rs:1669-1688` dispatches all four survey message types.
  2. `crates/app/src/app/survey_impl.rs:488-790` handles `handle_survey_request`, `handle_survey_response`, `handle_survey_start_collecting`, `handle_survey_stop_collecting`.
  3. `crates/app/src/app/survey_impl.rs:941-956` `verify_survey_signature` performs Ed25519 signature verification via `henyey_crypto::verify_from_raw_key`.
  4. `crates/app/src/app/survey_impl.rs:674-680` encrypts response bodies via `henyey_crypto::seal_to_curve25519_public_key`.
  5. `crates/app/src/app/survey_impl.rs:786-793` decrypts response bodies via `henyey_crypto::open_from_curve25519_secret_key`.
  6. Rate limiting is enforced via `survey.rs:268-356` (`SurveyMessageLimiter`) in the overlay crate.

### §12.3 — Drop-to-close 5 s delay (Full)
- **Claim**: Drop schedules `TCPPeer::shutdown` 5 s later to drain the final `ERROR_MSG`.
- **Rust**: `send_error_and_drop` (`manager/peer_loop.rs`) queues `Send(err)` then `OutboundMessage::ShutdownAfterError` on the same outbound channel. The loop flushes `Send(err)` to the socket first (FIFO), then the `ShutdownAfterError` arm calls `wait_error_drop_drain()`, which sleeps `ERROR_DROP_DRAIN_DELAY = 5s` before breaking. Plain `Shutdown` (idle/normal teardown) stays immediate. Mirrors `TCPPeer.cpp:835-862`.
- **Notes**: The 5 s sleep is per-peer-task and interruptible by node shutdown (`state.running` going false, polled every 100 ms), so overlay teardown is never delayed 5 s per peer. Covered by `test_error_drop_drains_before_close` and `test_error_drop_drain_interrupted_by_shutdown`.

### §13.4 — Crypto-error → ERR_DATA (Partial)
- **Claim**: any `xdr_runtime_error` or `CryptoError` during message receive triggers `ERR_DATA` and drop.
- **Rust**: XDR decode errors surface as `OverlayError::Message`, propagated to the peer loop which logs and drops the peer (`peer_loop.rs:1334-1366`). The error message is *not* specifically `ERR_DATA` — the connection simply terminates without sending an outbound `ERROR_MSG`.
- **Notes**: Drop happens; ERR_DATA is not transmitted. Lower-priority drift since the peer will see the TCP close.

### §12.1 — ERROR_MSG MAC handling (Partial)
- **Claim**: ERROR_MSG sent with zero seq+MAC pre-key, normal HMAC post-key.
- **Receive side (parity)**: `auth.rs:672-675` *always* skips MAC verification for incoming ERROR_MSG, regardless of whether keys are established. Matches stellar-core `Peer.cpp:1032-1035`.
- **Send side (divergence)**: Post-auth ERROR_MSG goes through `send_error_and_drop` (`peer_loop.rs:306-314`) → `OutboundMessage::Send` → `peer.send()` (`peer.rs:738-751`) → `auth.wrap_message()`, which applies normal MAC/sequence. Stellar-core exempts outbound ERROR_MSG from MAC in `Hmac.cpp:72-79`. Pre-auth errors use `send_raw` (`peer.rs:573`) which is correct (no MAC before keys).
- **Notes**: Partial because receive-side matches but outbound path diverges. The divergence is benign (peers accept MACed ERROR_MSG) but not spec-identical.

### §10.4 — Tick promote-inbound (Full)
- **Claim**: Step 8: "Promote inbound peers (open a parallel outbound connection to their address) to fill any leftover pending slots."
- **Rust**: `promote_inbound_peers()` (`manager/tick.rs`) runs strictly last in the tick loop. It enumerates authenticated inbound peers via `enumerate_promotable_inbound_peers()` — skipping those we already have an outbound to or under a dial cooldown — shuffles them, and dials their advertised listening address (rewritten into `PeerInfo.address` at Hello, `peer.rs:687-691`) via `connect_to_explicit_peer`. To keep promotion reachable under outbound saturation, `fill_outbound_slots` leaves `RESERVED_FOR_PROMOTION = 1` pending slot free when promotable inbound peers exist. Mirrors `OverlayManagerImpl.cpp:782-794`.
- **Notes**: Covered by `test_promote_inbound_dials_inbound_peer_address`, `test_promote_inbound_skips_existing_outbound`, `test_promote_inbound_respects_pending_budget`, `test_fill_outbound_reserves_promotion_slot`, `test_enumerate_promotable_excludes_outbound_only`.

### §10.4 — Random out-of-sync drop (Partial)
- **Claim**: Step 6: when `availableAuthSlots == 0` and out-of-sync ≥ 60 s, drop one random non-preferred outbound.
- **Status**: **Partial**. henyey implements the random out-of-sync drop path via `maybe_drop_random_peer` at `manager/tick.rs:589-670`. The function starts a cooldown timer (`OUT_OF_SYNC_RECONNECT_DELAY = 60s` at tick.rs:572), and after cooldown drops a random non-preferred outbound peer. Called from the tick loop at tick.rs:287-291. Comprehensively tested (`test_maybe_drop_random_peer_drops_after_cooldown`, `test_maybe_drop_random_peer_skips_preferred`, `test_maybe_drop_random_peer_only_drops_outbound`, etc.).
- **Divergence 1 — later cooldown start**: In stellar-core, `OverlayManagerImpl::tick()` enters `updateTimerAndMaybeDropRandomPeer(shouldDrop)` whenever `availablePendingSlots > 0` (`OverlayManagerImpl.cpp:764-766`), and the helper arms `mLastOutOfSyncReconnect` on the very first unsynced tick even when `shouldDrop == false` (`OverlayManagerImpl.cpp:604-635`). In henyey, the cooldown is only started once the node is out of sync **and** outbound peers are already full (`outbound_count >= max_outbound`, tick.rs:604-605,607-609). This means henyey starts the 60 s cooldown later than stellar-core in cases where the node goes out of sync before outbound slots are saturated.
- **Divergence 3 — missing pending-slot precondition**: In stellar-core, the random-drop path is only entered when `availableOutboundPendingSlots() > 0` (`OverlayManagerImpl.cpp:735-740,798-810`); if both authenticated outbound slots and pending slots are exhausted, stellar-core returns from `tick()` before reaching `updateTimerAndMaybeDropRandomPeer()`. In henyey, `maybe_drop_random_peer()` is called unconditionally each tick (tick.rs:291) and only checks `outbound_count >= max_outbound` (tick.rs:604-605). This means henyey can enter the random-drop path even when pending connection slots are exhausted — a case where stellar-core would skip the drop entirely. The divergence is non-consensus-affecting but may cause unnecessary disconnects when the node cannot immediately use the freed slot.
- **Divergence 2 — missing timer reset when all peers are preferred**: In stellar-core, when the cooldown elapses and the drop path fires, `mLastOutOfSyncReconnect` is reset to `mApp.getClock().now()` unconditionally (throttling subsequent drops), even if no non-preferred peer was found to drop (`OverlayManagerImpl.cpp:626-633`). In henyey, when `candidates.is_empty()` the function returns early without updating the timestamp (tick.rs:637-639). This means that on subsequent ticks henyey will re-enter the drop logic immediately (cooldown already expired, still no candidates), whereas stellar-core throttles to one attempt per 60 s interval regardless.
- **Notes**: All three divergences are non-consensus-affecting — they only influence connection churn timing under out-of-sync conditions. The core mechanism (cooldown → random non-preferred drop) is implemented correctly.

### §10.6 — PEERS sample size (Full)
- **Claim**: "Up to 50 entries" (XDR vector ≤ 100).
- **Status**: **Full**. `manager/mod.rs:85` defines `const MAX_PEERS_PER_MESSAGE: usize = 50;` and it is enforced at `mod.rs:1386`.

## Drift items (require human review)

1. **§5.4.4 / `GET_SCP_STATE(0)` vs `getMinLedgerSeqToAskPeers()`**: Rust always sends 0; spec expects a computed value. Likely a Rust gap (would benefit from app-callback for catchup state).

*Resolved: §4.2 (RFC 5531 bit 31) was previously listed as Drift on the mistaken premise that stellar-core repurposed the record-marking bit as an authentication flag. That was factually wrong about upstream — there is no `is_authenticated` field in `codec.rs`, and `TCPPeer::getIncomingMsgLength()` (`TCPPeer.cpp:679`) masks the bit and never inspects it. Corrected to `Full`; see #3776 / PR #3800. (Note: xdrpp's generic message-socket reader `msgsock.cc:86` and `srpc.cc:42` do branch on the bit, but stellar-core's overlay does not use that read path — `TCPPeer` does its own asio read plus `getIncomingMsgLength()` — so this is not evidence that core inspects the bit.)*

## Dangling Spec anchors

All 8 anchors point to live spec sections:
- `survey.rs:44,48,52` → §11 ✓
- `auth.rs:694` → §4.3 ✓
- `connection.rs:94` → §5.3 ✓
- `codec.rs:30,34,195` → §3.3 ✓

No dangling anchors. (Older codebase comments cite §5.4 as well — also valid.)

## Recommendations

1. **Medium — §5.4.4 `GET_SCP_STATE` min-ledger seq**: Add app callback or local state to compute `getMinLedgerSeqToAskPeers()`; currently sends 0.
2. **Low — §5.4.2 cert refresh**: Currently one cert per `AuthContext`; could move to a process-lifetime shared cert with 30-min refresh, but functionally equivalent and not consensus-affecting.
3. **Low — §13.4 explicit `ERR_DATA` on crypto failure**: Emit an outbound `ERROR_MSG(ERR_DATA, ...)` before dropping on XDR/HMAC decode errors, instead of silent close.

Resolved in #2802: §7.3 `releaseAssert(processed <= batch_size)` (hard `assert!`), §10.4 promote-inbound, §12.3 5 s drop-to-close drain, INV-O11 recv-side assert.
