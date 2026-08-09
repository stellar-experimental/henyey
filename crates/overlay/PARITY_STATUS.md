# stellar-core Parity Status

**Crate**: `henyey-overlay`
**Upstream**: `stellar-core/src/overlay/`
**Overall Parity**: 90%
**Last Updated**: 2026-06-19

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Authentication (PeerAuth, Hmac) | Full | HKDF key derivation, HMAC-SHA256 MAC |
| Peer Connection (Peer, TCPPeer) | Partial | Core handshake and I/O complete; rejected outbound peers may still receive SEND_MORE_EXTENDED/GET_SCP_STATE before ERR_LOAD; admin JSON absent |
| OverlayManager | Partial | Core peer lifecycle present; preferred-peer eviction happens at authenticated admission; some peer-list and stats accessors absent |
| Floodgate | Full | Message deduplication, ledger-based cleanup, capacity-bounded (henyey-specific) |
| FlowControl | Full | Capacity tracking, throttling, SCP-aware trimming, CapacityGuard RAII |
| ItemFetcher / Tracker | Full | Fetch lifecycle, retry, envelope tracking |
| BanManager | Full | In-memory + SQLite persistence, auto-ban escalation, time-limited bans |
| PeerManager | Full | SQLite persistence, backoff, type tracking, direction-filtered queries |
| TxAdverts / TxDemandsManager | N/A | Advert/demand scheduling owned by app crate (`tx_flooding.rs`); overlay handles transport only |
| SurveyManager | Partial | Owned by the app layer (`crates/app/src/survey.rs` `SurveyDataManager`/`SurveyState`/`SurveyMessageLimiter` + `crates/app/src/app/survey_impl.rs`); overlay handles transport only. Survey flow complete; JSON summary and limiter behavior simplified |
| OverlayMetrics | Full | Counters and timers for all message types |
| PeerBareAddress | Full | Mapped to PeerAddress in lib.rs |
| MessageCodec (framing) | Full | Length-prefix with XDR record-marking continuation bit (bit 31): always set on send, masked and ignored on receive |
| Error Handling | Full | ERR_LOAD load shedding, 100-byte truncation, send_error_and_drop |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `BanManager.h` / `BanManagerImpl.h` / `BanManagerImpl.cpp` | `ban_manager.rs` | Full match |
| `Floodgate.h` / `Floodgate.cpp` | `flood.rs` | Full match (BLAKE2b-256) |
| `FlowControl.h` / `FlowControl.cpp` | `flow_control.rs` | Includes capacity classes |
| `FlowControlCapacity.h` / `FlowControlCapacity.cpp` | `flow_control.rs` | Merged into one module |
| `Hmac.h` / `Hmac.cpp` | `auth.rs` | Integrated into AuthContext |
| `ItemFetcher.h` / `ItemFetcher.cpp` | `item_fetcher.rs` | Full match |
| `OverlayManager.h` / `OverlayManagerImpl.h` / `OverlayManagerImpl.cpp` | `manager.rs` | Core logic present |
| `OverlayMetrics.h` / `OverlayMetrics.cpp` | `metrics.rs` | Custom atomics vs medida |
| `OverlayUtils.h` / `OverlayUtils.cpp` | (inline in error.rs) | logErrorOrThrow equivalent |
| `Peer.h` / `Peer.cpp` | `peer.rs`, `connection.rs` | Partial; many message handlers in manager |
| `PeerAuth.h` / `PeerAuth.cpp` | `auth.rs` | Full match |
| `PeerBareAddress.h` / `PeerBareAddress.cpp` | `lib.rs` (PeerAddress) | Full match |
| `PeerDoor.h` / `PeerDoor.cpp` | `connection.rs` (Listener) | Full match |
| `PeerManager.h` / `PeerManager.cpp` | `peer_manager.rs` | Full match |
| `RandomPeerSource.h` / `RandomPeerSource.cpp` | `peer_manager.rs` | Merged into PeerManager |
| `SurveyManager.h` / `SurveyManager.cpp` | `app/src/survey.rs` + `app/src/app/survey_impl.rs` | Full match (owned by app layer; relay/crypto in `survey_impl.rs`) |
| `SurveyDataManager.h` / `SurveyDataManager.cpp` | `app/src/survey.rs` (`SurveyDataManager`) | Full match |
| `SurveyMessageLimiter.h` / `SurveyMessageLimiter.cpp` | `app/src/survey.rs` (`SurveyMessageLimiter`) | Simplified implementation |
| `TCPPeer.h` / `TCPPeer.cpp` | `peer.rs`, `connection.rs`, `codec.rs` | Split across modules |
| `Tracker.h` / `Tracker.cpp` | `item_fetcher.rs` | Merged into ItemFetcher |
| `TxAdverts.h` / `TxAdverts.cpp` | App crate `tx_flooding.rs` | Moved to app layer |
| `TxDemandsManager.h` / `TxDemandsManager.cpp` | App crate `tx_flooding.rs` | Moved to app layer |

## Component Mapping

### BanManager (`ban_manager.rs`)

Corresponds to: `BanManager.h`, `BanManagerImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BanManager::create()` | `BanManager::new_in_memory()` / `new_with_db()` | Full |
| `BanManager::dropAll()` | `BanManager::drop_and_create()` | Full |
| `banNode()` | `ban_node()` | Full |
| `unbanNode()` | `unban_node()` | Full |
| `isBanned()` | `is_banned()` | Full |
| `getBans()` | `get_bans()` | Full |

### Floodgate (`flood.rs`)

Corresponds to: `Floodgate.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Floodgate()` | `FloodGate::new()` / `with_ttl()` / `with_limits()` | Full |
| `clearBelow()` | `clear_below()` | Full (+ henyey-specific TTL expiry + queue compaction) |
| `addRecord()` | `record_inbound_relay()` / `record_local_broadcast()` | Full |
| `broadcast()` | `get_forward_peers()` + external send | Full |
| `getPeersKnows()` | `get_forward_peers()` | Full |
| `forgetRecord()` | `forget()` | Full |
| `shutdown()` | `clear()` | Full |
| (no equivalent) | FIFO eviction at capacity (`evict_to_target()`) | Henyey-specific DoS protection |

**Henyey-specific divergences:**
- **Capacity bound**: The `seen` map is bounded at 1M entries (configurable via `with_limits()`). When the cap is reached, oldest entries are evicted via a FIFO queue with generation tokens. stellar-core has no equivalent bound. This prevents OOM under adversarial traffic but may cause redundant re-forwarding of evicted messages under extreme load. Not consensus-affecting.
- **TTL expiry in clear_below**: Entries older than the TTL are removed during `clear_below()`, regardless of ledger sequence. stellar-core's `clearBelow()` is purely ledger-based.

### FlowControl (`flow_control.rs`)

Corresponds to: `FlowControl.h`, `FlowControlCapacity.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `FlowControl()` | `FlowControl::new()` | Full |
| `maybeReleaseCapacity()` | `maybe_release_capacity()` | Full |
| `handleTxSizeIncrease()` | `handle_tx_size_increase()` | Full |
| `addMsgAndMaybeTrimQueue()` | `add_msg_and_maybe_trim_queue()` | Full |
| `getNextBatchToSend()` | `get_next_batch_to_send()` | Full |
| `updateMsgMetrics()` | (inline in get_next_batch_to_send) | Full |
| `getNumMessages()` | (inline) | Full |
| `getMessagePriority()` | `MessagePriority::from_message()` | Full |
| `isSendMoreValid()` | `is_send_more_valid()` | Full |
| `beginMessageProcessing()` | `begin_message_processing()` | Full |
| `endMessageProcessing()` | `end_message_processing()` (released at consumer-drain via `FlowControlRelease`, #3625 Phase 1) | Full |
| `canRead()` | `can_read()` (wired as the `if can_read()`-gated `peer.recv()` select arm, #3642 Phase 2; throttle lifts on consumer-drain via a per-peer `Notify`, the async equivalent of `scheduleRead`) | Full |
| `noOutboundCapacityTimeout()` | `no_outbound_capacity_timeout()` | Full |
| `getFlowControlJsonInfo()` | `get_stats()` | Full |
| `setPeerID()` | `set_peer_id()` | Full |
| `maybeThrottleRead()` | `maybe_throttle_read()` | Full |
| `stopThrottling()` | `stop_throttling()` | Full |
| `isThrottled()` | `is_throttled()` | Full |
| `processSentMessages()` | `process_sent_messages()` | Full |
| `FlowControlCapacity::getMsgResourceCount()` | (inline) | Full |
| `FlowControlCapacity::getCapacityLimits()` | (in config) | Full |
| `FlowControlCapacity::lockOutboundCapacity()` | (in get_next_batch_to_send) | Full |
| `FlowControlCapacity::lockLocalCapacity()` | (in begin_message_processing) | Full |
| `FlowControlCapacity::releaseLocalCapacity()` | (in end_message_processing) | Full |
| `FlowControlCapacity::hasOutboundCapacity()` | (inline) | Full |
| `FlowControlCapacity::msgBodySize()` | `msg_body_size()` | Full |

### ItemFetcher / Tracker (`item_fetcher.rs`)

Corresponds to: `ItemFetcher.h`, `Tracker.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ItemFetcher()` | `ItemFetcher::new()` | Full |
| `fetch()` | `fetch()` | Full |
| `stopFetch()` | `stop_fetch()` | Full |
| `getLastSeenSlotIndex()` | `get_last_seen_slot_index()` | Full |
| `fetchingFor()` | `fetching_for()` | Full |
| `stopFetchingBelow()` | `stop_fetching_outside_range()` | Full |
| `doesntHave()` | `doesnt_have()` | Full |
| `recv()` | `recv()` | Full |
| `Tracker()` | `Tracker::new()` | Full |
| `Tracker::empty()` | `is_empty()` | Full |
| `Tracker::waitingEnvelopes()` | `waiting_envelopes()` | Full |
| `Tracker::size()` | `len()` | Full |
| `Tracker::pop()` | `pop()` | Full |
| `Tracker::getDuration()` | `get_duration()` | Full |
| `Tracker::clearEnvelopesBelow()` | `clear_envelopes_below()` | Full |
| `Tracker::listen()` | `listen()` | Full |
| `Tracker::discard()` | `discard()` | Full |
| `Tracker::cancel()` | `cancel()` | Full |
| `Tracker::doesntHave()` | `doesnt_have()` | Full |
| `Tracker::tryNextPeer()` | `try_next_peer()` | Full |
| `Tracker::getLastSeenSlotIndex()` | `last_seen_slot_index()` | Full |
| `Tracker::resetLastSeenSlotIndex()` | `reset_last_seen_slot_index()` | Full |

#### Intentional Divergences

| Feature | stellar-core | henyey | Rationale |
|---------|-------------|--------|-----------|
| Tracker map cap | No cap on `mTrackers` | `max_trackers` cap (default 512) | Defense-in-depth against unbounded memory growth under adversarial flooding. Does not activate under normal operation. See #2439. |
| Empty tracker removal | `stopFetch()` leaves empty trackers | `stop_fetch()` removes empty trackers | Prevents cap space poisoning; matches `recv()` behavior. |

### Peer (`peer.rs`, `connection.rs`)

Corresponds to: `Peer.h`, `TCPPeer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Peer()` constructor | `Peer::connect()` / `Peer::accept()` | Full |
| `initialize()` | (in connect/accept) | Full |
| `sendHello()` | (in handshake flow) | Full |
| `sendAuth()` | (in handshake flow) | Full |
| `recvHello()` | (in handshake flow via AuthContext) | Full |
| `recvAuth()` | (in handshake flow) | Full |
| `sendMessage()` | `send()` | Full |
| *send-type metrics (19 grouped meters)* | `record_send()` via `OverlayMessageKind` | Approximate — Henyey uses 21 individual counters (one per XDR variant) vs stellar-core's 19 grouped meters; henyey counts post-successful-send vs stellar-core pre-send |
| `recvMessage()` / `recvRawMessage()` | `recv()` | Full |
| `recvError()` | (in manager message dispatch) | Full |
| `recvPeers()` | (in manager message dispatch) | Full |
| `recvDontHave()` | (in MessageDispatcher) | Full |
| `recvSendMore()` | (in manager message dispatch) | Full |
| `recvGetTxSet()` | (in manager message dispatch) | Full |
| `recvTxSet()` / `recvGeneralizedTxSet()` | (in MessageDispatcher) | Full |
| `recvTransaction()` | (in manager via broadcast) — flow-control capacity released **inline** on the peer task via `CapacityGuard::finish()` (non-SCP path), matching core's synchronous `recvTransaction` + `~CapacityTrackedMessage`. NOT deferred to the lossy broadcast consumer (that would be a parity regression). Verified at parity #3643 Phase 3. | Full |
| `recvGetSCPQuorumSet()` | (in manager message dispatch) | Full |
| `recvSCPQuorumSet()` | (in MessageDispatcher) | Full |
| `recvSCPMessage()` | (in manager via broadcast) | Full |
| `recvGetSCPState()` | (in manager via broadcast) | Full |
| `recvFloodAdvert()` | (in manager, forwarded to app) | Full |
| `recvFloodDemand()` | (in manager, forwarded to app) | Full |
| `recvSurveyRequestMessage()` | (in manager via SurveyManager) | Full |
| `recvSurveyResponseMessage()` | (in manager via SurveyManager) | Full |
| `recvSurveyStartCollectingMessage()` | (in manager) | Full |
| `recvSurveyStopCollectingMessage()` | (in manager) | Full |
| `sendGetTxSet()` | `PeerSender::send()` | Full |
| `sendGetQuorumSet()` | `PeerSender::send()` | Full |
| `sendGetScpState()` | `PeerSender::send()` | Full |
| `sendErrorAndDrop()` | (in manager error handling) | Full |
| `sendTxDemand()` | (scheduled by app layer) | Full |
| `sendAdvert()` | (scheduled by app layer) | Full |
| `sendSendMore()` | `send_more()` / `send_more_extended()` | Full |
| `sendDontHave()` | (in manager message dispatch) | Full |
| `sendPeers()` | (in manager advertiser) | Full |
| `sendSCPQuorumSet()` | (in manager message dispatch) | Full |
| `sendError()` | (in manager) | Full |
| `drop()` | `close()` | Full |
| `getRole()` | `direction()` | Full |
| `getLifeTime()` | (via PeerStats.connected_at) | Full |
| `getPing()` | RTT tracked via GetScpQuorumset ping | Full |
| `getRemoteVersion()` | `info().remote_version` | Full |
| `getRemoteOverlayVersion()` | `info().overlay_version` | Full |
| `getAddress()` | `remote_addr()` | Full |
| `getPeerID()` | `id()` | Full |
| `toString()` | `Display` impl | Full |
| `getJsonInfo()` | N/A | None |
| `handleMaxTxSizeIncrease()` | `OverlayManager::handle_max_tx_size_increase()` | Full |
| `pingPeer()` | GetScpQuorumset ping every 5s in `run_peer_loop()` | Full |
| `maybeProcessPingResponse()` | DontHave response RTT tracking | Full |
| `startRecurrentTimer()` | 5s check in `run_peer_loop()` | Full |
| `recurrentTimerExpired()` | Idle/straggler timeout in `run_peer_loop()` (straggler keyed on `enqueue_time_of_last_write` = `mEnqueueTimeOfLastWrite` parity, #3625 Phase 3 #3643) | Full |
| `getIOTimeout()` | Idle/straggler timeout in `run_peer_loop()` | Full |
| `beginMessageProcessing()` | `FlowControl::begin_message_processing()` | Full |
| `endMessageProcessing()` | `FlowControl::end_message_processing()` | Full |
| `process()` (query throttle) | `QueryRateLimiter::check()` via `QueryKind` enum (GetTxSet, GetScpQuorumSet, GetScpState with fixed max=10) | Full |
| `canRead()` | `FlowControl::can_read()` — message track gates on `total_capacity`; byte track is unconditionally `true` (`total_capacity: None`), exactly matching `FlowControlByteCapacity::canRead` (`releaseAssert(!mTotalCapacity); return true;`). Verified at parity #3643 Phase 3 (no byte read-gate). | Full |
| `isConnected()` | `is_connected()` | Full |
| `isAuthenticated()` | `is_ready()` | Full |
| `PeerMetrics` struct | `PeerStats` struct | Full |
| `TimestampedMessage` | (implicit in codec/connection) | Full |
| `CapacityTrackedMessage` | `CapacityGuard` RAII in `flow_control.rs` | Full |
| `TCPPeer::initiate()` | `Peer::connect()` | Full |
| `TCPPeer::accept()` | `Peer::accept()` | Full |
| `TCPPeer::drop()` | `Peer::close()` | Full |
| `TCPPeer::sendMessage()` | `Connection::send()` | Full |
| `TCPPeer::recvMessage()` | `Connection::recv()` | Full |
| `TCPPeer::connected()` | (implicit in connect) | Full |
| `TCPPeer::scheduleRead()` | (implicit in tokio) | Full |
| `TCPPeer::messageSender()` | (implicit in async write) | Full |
| `TCPPeer::writeHandler()` | (implicit in tokio) | Full |
| `TCPPeer::readHeaderHandler()` | (in MessageCodec decoder) | Full |
| `TCPPeer::readBodyHandler()` | (in MessageCodec decoder) | Full |
| `TCPPeer::shutdown()` | `close()` | Full |
| `cancelTimers()` | N/A (Tokio handles timer lifecycle) | Full |
| `msgSummary()` | `helpers::message_type_name()` | Full |
| `doIfAuthenticated()` | N/A (different concurrency model) | Full |

### PeerAuth / AuthContext (`auth.rs`)

Corresponds to: `PeerAuth.h`, `Hmac.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerAuth()` | `AuthContext::new()` | Full |
| `getAuthCert()` | `AuthCert::new()` | Full |
| `verifyRemoteAuthCert()` | `AuthCert::verify()` | Full |
| `getSendingMacKey()` | `derive_mac_keys()` (send half) | Full |
| `getReceivingMacKey()` | `derive_mac_keys()` (recv half) | Full |
| `getSharedKey()` | (inline in derive_mac_keys) | Full |
| `Hmac::setSendMackey()` | (set in process_hello) | Full |
| `Hmac::setRecvMackey()` | (set in process_hello) | Full |
| `Hmac::checkAuthenticatedMessage()` | `unwrap_message()` | Full |
| `Hmac::setAuthenticatedMessageBody()` | `wrap_message()` | Full |

### PeerBareAddress (`lib.rs`)

Corresponds to: `PeerBareAddress.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerBareAddress()` | `PeerAddress::new()` | Full |
| `resolve()` | (DNS resolution in connect flow) | Full |
| `isEmpty()` | N/A (Rust type always valid) | Full |
| `getIP()` | `.host` field | Full |
| `getPort()` | `.port` field | Full |
| `toString()` | `Display::fmt()` | Full |
| `isPrivate()` | `is_private()` | Full |
| `isLocalhost()` | (covered by is_private) | Full |
| `operator==` / `operator<` | `PartialEq` / `Eq` / `Hash` derives | Full |

### PeerDoor (`connection.rs`)

Corresponds to: `PeerDoor.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerDoor()` | `Listener::bind()` | Full |
| `start()` | (bind returns listening socket) | Full |
| `close()` | (drop Listener) | Full |
| `acceptNextPeer()` | `Listener::accept()` | Full |
| `handleKnock()` | (in manager accept flow) | Full |

### PeerManager (`peer_manager.rs`)

Corresponds to: `PeerManager.h`, `RandomPeerSource.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerManager()` | `PeerManager::new_in_memory()` / `new_with_db()` | Full |
| `dropAll()` | `clear_all()` | Full |
| `ensureExists()` | `ensure_exists()` | Full |
| `update()` (type) | `update_type()` | Full |
| `update()` (backoff) | `update_backoff()` | Full |
| `update()` (both) | `update()` | Full |
| `load()` | `load()` | Full |
| `store()` | `store()` | Full |
| `loadRandomPeers()` | `load_random_peers()` | Full |
| `removePeersWithManyFailures()` | `remove_peers_with_many_failures()` | Full |
| `getPeersToSend()` | `get_peers_to_send()` | Full |
| `loadAllPeers()` | `get_all_peers()` | Full |
| `storePeers()` | (via store) | Full |
| `RandomPeerSource::maxFailures()` | (inline in query construction) | Full |
| `RandomPeerSource::nextAttemptCutoff()` | (inline in query construction) | Full |
| `RandomPeerSource::getRandomPeers()` | `load_random_peers()` | Full |

### OverlayManager (`manager.rs`)

Corresponds to: `OverlayManager.h`, `OverlayManagerImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `OverlayManagerImpl()` | `OverlayManager::new()` | Full |
| `start()` | `start()` | Full |
| `shutdown()` | `shutdown()` | Full |
| `isShuttingDown()` | `is_running()` (inverted) | Full |
| `clearLedgersBelow()` | `clear_ledgers_below()` | Full |
| `broadcastMessage()` | (via broadcast channel + flood gate) | Full |
| `recvFloodedMsgID()` | `FloodGate::record_inbound_relay()` / `record_local_broadcast()` | Full |
| `recvTransaction()` | (via broadcast channel + flood gate) | Full |
| `forgetFloodedMsg()` | `forget_flooded_msg()` — called on SCP discard and tx rejection; also cleared via `clear_below()` at ledger close | Full |
| `recvTxDemand()` | Handled in app crate (`App::handle_flood_demand`) | Full (moved to app layer) |
| `getRandomAuthenticatedPeers()` | (shuffled peer list) | Full |
| `getRandomInboundAuthenticatedPeers()` | N/A | None |
| `getRandomOutboundAuthenticatedPeers()` | N/A | None |
| `getConnectedPeer()` | (via DashMap lookup) | Full |
| `getMoreSCPState()` (bounded 2-peer pull) | `request_scp_state()` — selects up to 2 random authenticated peers | Full |
| (no upstream analog) | `request_scp_state_widened()` — bounded wider GetScpState pull, henyey-specific recovery escape (#3318) | Deviation (see Architectural Differences) |
| `maybeAddInboundConnection()` | (in listener accept flow) | Full |
| `addOutboundConnection()` | (in connector flow) | Full |
| `removePeer()` | (via peer drop) | Full |
| `acceptAuthenticatedPeer()` | `try_accept_authenticated_peer()` after handshake completion | Partial (preferred eviction, load rejection, and `PREFERRED_PEERS_ONLY` strict mode match admission semantics; outbound rejection still occurs after SEND_MORE_EXTENDED/GET_SCP_STATE) |
| `isPreferred()` | `PreferredPeerSet::is_preferred()` | Full (config hostname + resolved IP matching + `PREFERRED_PEER_KEYS` identity matching) |
| `isPossiblyPreferred()` | `ConnectionPool::try_reserve_with_ip()` | Full (runtime update via `update_preferred_ips()`) |
| `haveSpaceForConnection()` | `ConnectionPool::can_accept()` | Full |
| `getInboundPendingPeers()` | N/A | None |
| `getOutboundPendingPeers()` | N/A | None |
| `getPendingPeers()` | N/A | None |
| `getLiveInboundPeersCounter()` | (via ConnectionPool.count) | Full |
| `getPendingPeersCount()` | N/A | None |
| `getInboundAuthenticatedPeers()` | N/A | None |
| `getOutboundAuthenticatedPeers()` | N/A | None |
| `getAuthenticatedPeers()` | `authenticated_peers()` | Full |
| `getAuthenticatedPeersCount()` | `peer_count()` | Full |
| `connectTo()` | (in connector flow) | Full |
| `getPeersKnows()` | `FloodGate::get_forward_peers()` | Full |
| `getOverlayMetrics()` | (via OverlayMetrics) | Full |
| `getPeerAuth()` | (via AuthContext per peer) | Full |
| `getPeerManager()` | N/A (not exposed directly) | Partial |
| `getSurveyManager()` | N/A (not exposed directly) | Partial |
| `recordMessageMetric()` | (via OverlayMetrics) | Full |
| `getFlowControlBytesTotal()` | `FlowControlBytesConfig::bytes_total()` | Full |
| `getFlowControlBytesBatch()` | `FlowControlBytesConfig::bytes_batch()` | Full |
| `checkScheduledAndCache()` | `ScpScheduledCache` in henyey-app with RAII token lifetime (#2631) | Full |
| `getOverlayThreadSnapshot()` | N/A | None |
| `tick()` | `start_tick_loop()` (3s interval) | Partial — calls `maybe_drop_random_peer()` unconditionally each tick; upstream only enters the random-drop path when `availableOutboundPendingSlots() > 0` (i.e. pending slots are available), skipping `updateTimerAndMaybeDropRandomPeer()` entirely when pending slots are exhausted |
| `updateTimerAndMaybeDropRandomPeer()` | `maybe_drop_random_peer()` | Partial |
| `storeConfigPeers()` | In `start()` — stores known+preferred peers | Full |
| `purgeDeadPeers()` | App layer `maintain_peers()` — `remove_peers_with_failures(120)` | Full |
| `triggerPeerResolution()` | DNS backoff in tick loop | Full |
| `resolvePeers()` | DNS resolution with exponential backoff; results update both known peers and `PreferredPeerSet` (resolved IPs → inbound pool) | Full |
| `storePeerList()` | In `start()` and tick loop | Full |
| `connectToImpl()` | (in connector flow) | Full |
| `moveToAuthenticated()` | `ConnectionPool` promotion via centralized admission | Full |
| `nonPreferredAuthenticatedCount()` | `count_non_preferred_outbound_peers()` | Full |
| `updateSizeCounters()` | N/A | None |
| `shufflePeerList()` | (via rand::shuffle) | Full |
| `canAcceptOutboundPeer()` | (via ConnectionPool) | Full |
| `isFloodMessage()` | `helpers::is_flood_message()` | Full |
| `createTxBatch()` | N/A | None |
| `getFlowControlBytesBatch()` | N/A | None |
| `availableOutboundAuthenticatedSlots()` | N/A | None |
| `getPeersToConnectTo()` | (in connector flow) | Partial |

### OverlayMetrics (`metrics.rs`)

Corresponds to: `OverlayMetrics.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `OverlayMetrics()` | `OverlayMetrics::new()` | Full |
| All meter/timer/counter fields | Matching Counter/Timer fields | Full |

### SurveyManager (`app/src/survey.rs` + `app/src/app/survey_impl.rs`)

Corresponds to: `SurveyManager.h`, `SurveyDataManager.h`, `SurveyMessageLimiter.h`

Survey is owned by the **app layer** — the data model lives in `crates/app/src/survey.rs` (`SurveyDataManager` / `SurveyState` / `SurveyMessageLimiter`) and the relay/crypto/wire handling in `crates/app/src/app/survey_impl.rs`. (The former overlay `survey.rs` `SurveyManager` shell was a dead, never-constructed duplicate and was removed in #3543.) Rust method names below refer to the app-layer symbols.

| stellar-core | Rust | Status |
|--------------|------|--------|
| `SurveyManager()` | `SurveyDataManager::new()` / `SurveyState::new()` | Full |
| `startSurveyReporting()` | `start_collecting()` | Full |
| `stopSurveyReporting()` | `stop_collecting()` | Full |
| `addNodeToRunningSurveyBacklog()` | `add_peer_to_backlog()` | Full |
| `relayOrProcessResponse()` | `survey_impl::handle_survey_response()` | Full |
| `relayOrProcessRequest()` | `survey_impl::handle_survey_request()` | Full |
| `clearOldLedgers()` | `clear_old_ledgers()` | Full |
| `getJsonResults()` | `get_node_data()` / peer data getters; the compat HTTP `/getsurveyresult` handler (`app::compat_http`) now projects the survey report to the stellar-core `getJsonResults()` JSON shape (`surveyInProgress`, strkey `backlog`/`badResponseNodes`, `topology` map) (#3298) | Partial |
| `broadcastStartSurveyCollecting()` | `survey_impl::handle_survey_start_collecting()` | Full |
| `relayStartSurveyCollecting()` | `survey_impl::handle_survey_start_collecting()` | Full |
| `broadcastStopSurveyCollecting()` | `survey_impl::handle_survey_stop_collecting()` | Full |
| `relayStopSurveyCollecting()` | `survey_impl::handle_survey_stop_collecting()` | Full |
| `modifyNodeData()` | `modify_node_data()` | Full |
| `modifyPeerData()` | `modify_peer_data()` | Full |
| `recordDroppedPeer()` | `record_dropped_peer()` | None |
| `updateSurveyPhase()` | `update_phase()` | Full |
| `sendTopologyRequest()` | `survey_impl::handle_survey_request()` | Full |
| `processTimeSlicedTopologyResponse()` | `survey_impl::handle_survey_response()` | Full |
| `processTimeSlicedTopologyRequest()` | `survey_impl::handle_survey_request()` | Full |
| `populateSurveyResponseMessage()` | `survey_impl::handle_survey_response()` | Full |
| `populateSurveyRequestMessage()` | `survey_impl::handle_survey_request()` | Full |
| `dropPeerIfSigInvalid()` | `survey_impl::verify_survey_signature()` | Full |
| `surveyorPermitted()` | `surveyor_permitted()` | Full |
| `getMsgSummary()` | N/A | None |
| `SurveyDataManager::startSurveyCollecting()` | `start_collecting()` | Full |
| `SurveyDataManager::stopSurveyCollecting()` | `stop_collecting()` | Full |
| `SurveyDataManager::modifyNodeData()` | `modify_node_data()` | Full |
| `SurveyDataManager::modifyPeerData()` | `modify_peer_data()` | Full |
| `SurveyDataManager::recordDroppedPeer()` | `record_dropped_peer()` | None |
| `SurveyDataManager::getNonce()` | `nonce()` | Full |
| `SurveyDataManager::nonceIsReporting()` | (via phase check) | Full |
| `SurveyDataManager::fillSurveyData()` | `survey_impl::handle_survey_request()` | Full |
| `SurveyDataManager::getFinalNodeData()` | `get_node_data()` | Full |
| `SurveyDataManager::getFinalInboundPeerData()` | `get_inbound_peer_data()` | Full |
| `SurveyDataManager::getFinalOutboundPeerData()` | `get_outbound_peer_data()` | Full |
| `SurveyDataManager::surveyIsActive()` | `is_active()` | Full |
| `SurveyDataManager::updateSurveyPhase()` | `update_phase()` | Full |
| `SurveyMessageLimiter::addAndValidateRequest()` | `add_request()` | Partial |
| `SurveyMessageLimiter::recordAndValidateResponse()` | `record_response()` | Partial |
| `SurveyMessageLimiter::clearOldLedgers()` | `clear_old_ledgers()` | Full |
| `SurveyMessageLimiter::validateStartSurveyCollecting()` | `survey_impl::handle_survey_start_collecting()` | Full |
| `SurveyMessageLimiter::validateStopSurveyCollecting()` | `survey_impl::handle_survey_stop_collecting()` | Full |

### MessageCodec (`codec.rs`)

Corresponds to: XDR framing in `TCPPeer.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| RM framing (4-byte header) | `MessageCodec` (Decoder+Encoder) | Full |
| RM continuation bit (bit 31) — `getIncomingMsgLength()` masks it (`length &= 0x7f`) and never inspects it; the writer (xdrpp `marshal.cc`) always sets it | `MessageFrame::is_last_fragment` (descriptive metadata only; masked, never rejected on) | Full |
| `MAX_UNAUTH_MESSAGE_SIZE` | `MIN_MESSAGE_SIZE` / `MAX_MESSAGE_SIZE` | Full |

### PeerSharedKeyId (`N/A`)

Corresponds to: `PeerSharedKeyId.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PeerSharedKeyId` struct | N/A (different cache approach) | N/A |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `LoopbackPeer` (test/LoopbackPeer.h) | Test-only construct for in-process simulation |
| `OverlayTestUtils` (test/OverlayTestUtils.h) | Test utilities, not production code |
| `PeerSharedKeyId` | Different caching approach in Rust; key cache not needed |
| `StellarXDR.h` | Convenience header; handled by stellar-xdr crate |
| `VirtualClock` / `VirtualTimer` integration | Tokio provides async timers natively |
| `BACKGROUND_OVERLAY_PROCESSING` mode | Tokio async model is inherently parallel |
| `getOverlayThreadSnapshot()` | Different concurrency model; no separate overlay thread |
| `BUILD_TESTS`-only methods | Test helpers; Rust uses different test patterns |
| `OverlayUtils::logErrorOrThrow()` | Handled by Rust's tracing + Result types |
| `recvTxBatch()` (BUILD_TESTS only) | Test-only message handler |
| `PeerAuth` certificate refresh timer | Per-connection `AuthContext` generates a fresh 1-hour cert; connections time out long before expiry |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `Peer::getJsonInfo()` | Low | JSON info for admin API |
| `OverlayManagerImpl::getRandomInboundAuthenticatedPeers()` | Low | Separate inbound peer list |
| `OverlayManagerImpl::getRandomOutboundAuthenticatedPeers()` | Low | Separate outbound peer list |
| `OverlayManagerImpl::getInboundPendingPeers()` | Low | Pending peer tracking |
| `OverlayManagerImpl::getOutboundPendingPeers()` | Low | Pending peer tracking |
| `OverlayManagerImpl::getPendingPeers()` | Low | Combined pending peer list |
| `OverlayManagerImpl::getPendingPeersCount()` | Low | Pending count |
| `OverlayManagerImpl::getInboundAuthenticatedPeers()` | Low | Separate inbound map |
| `OverlayManagerImpl::getOutboundAuthenticatedPeers()` | Low | Separate outbound map |
| `OverlayManagerImpl::createTxBatch()` | Low | Batch TX message creation |
| `OverlayManagerImpl::getFlowControlBytesBatch()` | Low | Config-based batch size |
| `OverlayManagerImpl::nonPreferredAuthenticatedCount()` | Low | Count for peer eviction |
| `OverlayManagerImpl::updateSizeCounters()` | Low | Metrics for pending/auth sizes |
| `OverlayManagerImpl::availableOutboundAuthenticatedSlots()` | Low | Slot availability check |
| `SurveyManager::getMsgSummary()` | Low | Survey message logging |
| `SurveyDataManager::recordDroppedPeer()` | Medium | `record_dropped_peer()` exists with correct internal semantics but has **zero production callers** — it is never invoked from the peer-drop path, so henyey survey responses always report `dropped_peers: 0`. The symmetric add/node-data counters (`record_added_peer` / `modify_node_data`) are likewise production-unwired. Wiring these into the peer lifecycle is a behavior-changing parity task tracked in #3500. |

## Architectural Differences

1. **Async Runtime**
   - **stellar-core**: ASIO with callbacks, VirtualClock for timers, single main thread with optional background thread
   - **Rust**: Tokio async/await with native timers, tasks run on Tokio runtime
   - **Rationale**: Tokio provides equivalent async I/O with a more modern ergonomic model; inherently supports concurrent message processing without explicit threading

2. **Message Routing**
   - **stellar-core**: Messages dispatched in Peer class via virtual method calls (recvHello, recvAuth, etc.)
   - **Rust**: Messages received by Peer, then routed through OverlayManager which dispatches to MessageDispatcher, broadcast channel, or component-specific handlers
   - **Rationale**: Decouples message handling from connection management; makes testing easier

3. **Peer Lifecycle Management**
   - **stellar-core**: PeersList with pending/authenticated separation, shared_ptr/weak_ptr ownership, explicit CLOSING state
   - **Rust**: DashMap of Arc-wrapped peers, ConnectionPool for slot management, PeerState enum
   - **Rationale**: DashMap provides concurrent access without global locks; Arc handles ownership naturally

4. **Metrics System**
   - **stellar-core**: Medida library (timers, meters, counters, histograms)
   - **Rust**: Custom atomics-based Counter and Timer types in metrics.rs
   - **Rationale**: Avoids external metrics library dependency; atomics provide thread-safe counting with lower overhead

5. **Survey Encryption**
   - **stellar-core**: Survey response encryption handled inline in SurveyManager
   - **Rust**: Encryption/decryption handled at application layer (henyey-app) using henyey_crypto
   - **Rationale**: Separation of concerns; crypto operations belong at a higher level

6. **Wider peer-SCP recovery pull (`request_scp_state_widened`, #3318)** — henyey-specific deviation
   - **stellar-core**: `HerderImpl::getMoreSCPState()` is hard-bounded to 2 random authenticated peers (via `getRandomAuthenticatedPeers`) and NEVER widens. Out-of-sync recovery self-heals by re-selecting fresh 2 peers every 10s at the low watermark.
   - **Rust**: The steady-state path (`request_scp_state`) is an EXACT mirror of upstream (still exactly 2 peers, full inbound+outbound authenticated set, every recovery tick). On top of that, `request_scp_state_widened()` adds ONE bounded wider GetScpState pull — driven by the app layer (`App::maybe_widen_near_tip_scp_pull`, crate:app) — that fires only when the near-tip / archive-confirmed-behind recovery loop has had ZERO serviceable peers (`peers_could_serve(watermark) == 0`) for longer than the 120s wall-clock deadline (the #2789 stuck-onset clock). Fan-out is bounded to `min(serviceable, 8)`, falling back to ALL authenticated peers when serviceable == 0; the send is non-blocking (`try_send_to`).
   - **Rationale**: The first sustained non-self-healing production wedge (build `b90b29f7`, 06:11Z) showed that when the connected set is collectively too far behind / too thin (e.g. the thin-inbound regime of #3419), the fixed 2-peer pull re-lands on unserviceable peers forever and the node wedges (lcl frozen, recovery attempts 8→74, manual restart). The wider pull lets the node obtain the back-fill instead of requiring an operator restart. It is strictly additive, provably gated (cannot fire while any peer can serve, cannot fire before the deadline), and never alters steady-state recovery — an EXACT upstream mirror outside the wedge condition. **Operator-approved** deviation (sign-off 2026-06-22). Residual: if the peer set genuinely holds no copy of the slot, even the widened pull reaches 0 serviceable peers and the node still cannot self-serve — an inherent limit (operator-restart status quo), not a defect of this escape.

7. **Bounded SCP ingest channel (`scp_message_tx`/`scp_message_rx`, #3623)** — henyey-specific approximation of core's inbound flow control
   - **stellar-core**: Processes SCP synchronously on the main thread under a bounded inbound flow-control capacity (`FlowControlMessageCapacity::canRead`, `Peer::recvMessage` → `recvSCPMessage`). A sending peer cannot exceed the receiver's negotiated inbound capacity, and capacity is only released as messages are consumed — so when the main loop stalls, peers stop being granted send-more and back off. There is **no unbounded async queue** between overlay receive and herder processing.
   - **Rust**: SCP envelopes cross from the peer-receive path to the main event loop over a dedicated `tokio::sync::mpsc` channel. This channel is **bounded** at `SCP_CHANNEL_CAPACITY` (8192). On overflow the overlay side `try_send`s and **drops** the envelope (counted in `messages_dropped`) rather than blocking the peer path. Dropped SCP is recoverable: peers re-flood every slot and the event loop's gap-detection + `SyncRecoveryManager` backfill missing state via `GetScpState`.
   - **Rationale**: Originally an `mpsc::unbounded_channel`, which had no core analog. When the event loop stalled (#3582, post-catchup SQLite write-lock contention), ~24 validators flooded ~100+ envelopes/slot with nothing draining, growing RSS ~4 GB/min until OOM-kill — a fatal restart loop (#3623). The fixed-capacity drop-on-full channel achieves the essential property core guarantees (bounded inbound memory) without changing which envelopes are *processed*, moving henyey toward parity.
   - **#3625 Phase 1 (drain-gated SEND_MORE)**: SEND_MORE credit is now released on **consumer drain**, not channel enqueue. The per-peer `begin_message_processing` capacity lock taken on the peer-receive task is carried as a `FlowControlRelease` token on the SCP-routed `OverlayMessage`; `end_message_processing` fires (and a `SEND_MORE_EXTENDED` is granted at the 40-message / byte batch boundary) only when the app event-loop consumer drains the envelope (`pump_scp_intake`). A stalled consumer therefore stops granting SEND_MORE, back-pressuring senders that honor outbound capacity — matching core's "release per processed message" model. The release is idempotent and fires on every drain/drop path (including the #3623 drop-on-full backstop, which is **retained as defense-in-depth**), so inbound credit never leaks.
   - **#3642 Phase 2 (read-side socket throttle)**: `can_read()` is now wired into the per-peer read loop. The `peer.recv()` arm of `run_peer_loop`'s `tokio::select!` is gated `if flow_control.can_read()`, so a peer whose inbound total reading capacity (`PEER_READING_CAPACITY = 201`) is exhausted by un-drained SCP envelopes stops pulling bytes — back-pressuring the sender at the TCP window exactly as core's `TCPPeer::maybeThrottleRead` declining to reschedule the read does. After each handled message the loop calls `maybe_throttle_read()` (records `last_throttle` when `!can_read()`, mirroring core's post-`recvMessage` call at `TCPPeer.cpp:620/770`). The throttle is lifted by the Phase-1 consumer-drain release: each SCP `FlowControlRelease` carries a clone of a per-peer `tokio::sync::Notify`, and on the release that completes a full reading-capacity batch (`SendMoreCapacity::num_total_messages > 0`) it calls `stop_throttling()` then `notify_one()`, waking a dedicated select arm that re-enables the gated read — the async equivalent of core's `Peer::endMessageProcessing` → `stopThrottling()` + `scheduleRead()` gated on `isThrottled() && numTotalMessages > 0` (`Peer.cpp:313-333`). A 1s periodic-tick re-evaluation backstops any missed wake. The fetch backfill (TxSet / GeneralizedTxSet / ScpQuorumset / DontHave) is **not** flow-controlled and rides the separate unbounded `fetch_response` channel, so it is never throttled — the throttled SCP socket can never starve the fetch the consumer depends on (no self-wedge). Overlay-only footprint; `crates/app` is unchanged.
   - **#3643 Phase 3 (straggler parity + scope verification + backstop keep-decision)**: Re-derivation against pinned core v26.0.1 + `origin/main` resolved the remaining Phase-3 scope:
     - **Outbound straggler timeout — fixed to enqueue-time parity.** Core keys the straggler on `(now - mEnqueueTimeOfLastWrite) >= PEER_STRAGGLER_TIMEOUT (120s)` (`Peer.cpp:462`), where `mEnqueueTimeOfLastWrite` is reassigned each iteration of `messageSender`'s write loop (`TCPPeer.cpp:329`, last-wins = enqueue time of the **newest** message in the written FIFO prefix; init `now()` at ctor `Peer.cpp:144`). henyey previously keyed it on `last_write` (completed-write time, which resets to `now()` on every write) — so a peer writing steadily but unable to keep up was never flagged. Now a loop-local `enqueue_time_of_last_write: Instant` (init `now()`) is advanced — on each non-empty `send_flow_controlled_batch` and SEND_MORE drain — to the **MAX `time_emplaced`** over the messages actually written (`BatchSendOutcome.newest_emplaced`; henyey's batch is priority-interleaved via `build_next_batch`, not a contiguous FIFO slice, so MAX-emplaced is the faithful analogue of core's last-in-prefix), and the straggler branch of `check_peer_timeouts` keys on it. Idle (`last_read`/`last_write`, 30s) and no-outbound-capacity (`no_outbound_capacity_timeout`, 60s) stay **distinct** signals — no conflation.
     - **Byte-capacity `can_read` — already at parity (verified, test-locked).** The byte track has no `total_capacity` and never gates reads, matching `FlowControlByteCapacity::canRead`. No byte read-gate was added (that would diverge).
     - **tx/flood inline release — already at parity (verified, test-locked).** Non-SCP messages release capacity inline via `CapacityGuard::finish()` on the peer task, matching core's synchronous `recvTransaction`. NOT deferred to the lossy broadcast consumer.
   - **#3626 backstop — KEPT as an intentional core-divergence (no core equivalent).** Core has no aggregate inbound channel (per-peer synchronous processing). The #3625 Phase-2 per-peer read throttle bounds in-flight to `PEER_READING_CAPACITY (201)` **per peer**, but NOT the aggregate: worst-case `max_peers × 201 = 84 × 201 = 16 884 > 8 192 (SCP_CHANNEL_CAPACITY)`. Retiring the bounded channel (= unbounding `scp_message_tx`) is therefore NOT provably memory-safe offline; the backstop (bounded channel + `try_send` drop-on-full) is retained. Retirement / re-bound is **operator-gated #3649**.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Overlay/Peer | 40 TEST_CASE / 87 SECTION | 36 #[test] (manager/) + 2 (peer.rs) + 5 (connection.rs) | Better coverage now, but upstream handshake matrix is still broader |
| Flood | 1 TEST_CASE / 17 SECTION | 8 #[test] | Good single-module coverage |
| FlowControl | Embedded in `OverlayTests.cpp` | 23 #[test] | Strong direct unit coverage |
| ItemFetcher | 2 TEST_CASE / 16 SECTION | 12 #[test] + 8 integration | Good parity coverage |
| Tracker | 1 TEST_CASE / 13 SECTION | Covered in `item_fetcher.rs` and integration tests | Adequate |
| PeerManager | 8 TEST_CASE / 38 SECTION | 9 #[test] | Moderate gap remains in persistence edge cases |
| BanManager | No dedicated upstream test file | 15 #[test] | Rust coverage is stronger than upstream organization suggests |
| TCPPeer / framing | 4 TEST_CASE / 5 SECTION | 18 #[test] (codec.rs) + 5 (connection.rs) | Good framing coverage; fewer end-to-end socket scenarios |
| SurveyManager | 5 TEST_CASE / 7 SECTION | `#[test]` in app layer (`crates/app/src/survey.rs`) | Good unit coverage; survey is owned by the app layer |
| SurveyMessageLimiter | 1 TEST_CASE / 10 SECTION | Included in the app-layer survey tests (`crates/app/src/survey.rs`) | Core limiter paths covered |
| OverlayManager | 4 TEST_CASE / 0 SECTION | 36 #[test] | Strong unit coverage for startup, peer rotation, and bookkeeping |
| OverlayTopology | 2 TEST_CASE / 7 SECTION | 0 | Not covered |
| MessageDispatcher | N/A | 10 #[test] | Rust-specific; includes audit-002 cache bound tests |
| Metrics | N/A | 12 #[test] | Rust-specific |
| Auth | Covered indirectly in `OverlayTests.cpp` | 24 #[test] | Strong direct unit coverage |
| Codec | Covered by `TCPPeerTests.cpp` and `OverlayTests.cpp` | 18 #[test] | Good coverage |

### Test Gaps

- **Peer handshake and connection lifecycle**: Upstream still has the broadest end-to-end matrix in `OverlayTests.cpp` and `TCPPeerTests.cpp`, especially around version negotiation, malformed traffic, and timeout behavior.
- **PeerManager persistence**: Upstream has 8 TEST_CASE with 38 SECTION covering database serialization, selection, and backoff combinations; Rust has solid coverage but fewer scenario permutations.
- **Multi-node topology**: Upstream has OverlayTopologyTests with 2 TEST_CASE / 7 SECTION for multi-node overlay scenarios. Rust has none.
- **Network error handling**: Upstream socket-level tests still cover more malformed-frame and live-connection failure scenarios than the current Rust suite.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 255 |
| Gaps (None + Partial) | 28 |
| Intentional Omissions | 10 |
| **Parity** | **255 / (255 + 28) = 90%** |
