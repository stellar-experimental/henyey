## Pseudocode: crates/app/src/app/tx_flooding.rs

### Helper: tx_set_start_index

```
GUARD peers_len == 0 → 0
start = first 8 bytes of hash as little-endian u64
base = start % peers_len
→ (base + (peer_offset % peers_len)) % peers_len
```

### enqueue_tx_advert

```
hash = tx_hash(tx_envelope)
GUARD hash computed → return

GUARD hash not in tx_advert_set → return
tx_advert_set.insert(hash)
tx_advert_queue.push(hash)
```

### flush_tx_adverts

```
hashes = take all from tx_advert_queue
GUARD hashes not empty → return
clear tx_advert_set

GUARD overlay available → return
max_advert_size = max_advert_size()
snapshots = overlay.peer_snapshots()
GUARD snapshots not empty → return

peer_ids = extract peer_ids from snapshots
peer_set = set of peer_ids

"Remove adverts for disconnected peers"
adverts_by_peer.retain where peer in peer_set

"Build per-peer outgoing advert lists"
per_peer = []
for each peer_id in peer_ids:
  adverts = adverts_by_peer[peer_id]
  outgoing = []
  for each hash in hashes:
    if adverts.seen_advert(hash) → skip
    append hash to outgoing
  if outgoing not empty:
    append (peer_id, outgoing) to per_peer

"Send adverts in chunks"
for each (peer_id, hashes) in per_peer:
  for each chunk of size max_advert_size:
    advert = FloodAdvert { tx_hashes: chunk }
    overlay.send_to(peer_id, FloodAdvert(advert))
```

**Calls**: [max_advert_size](#helper-max_advert_size)

### flood_advert_period

```
→ max(config.flood_advert_period_ms, 1) as Duration
```

### flood_demand_period

```
→ max(config.flood_demand_period_ms, 1) as Duration
```

### Helper: flood_demand_backoff_delay

```
→ max(config.flood_demand_backoff_delay_ms, 1) as Duration
```

### Helper: max_advert_queue_size

```
→ max(herder.max_tx_set_size(), 1)
```

### Helper: max_advert_size

```
CONST TX_ADVERT_VECTOR_MAX_SIZE = 1000
ledger_close_ms = max(herder.ledger_close_time() * 1000, 1)
ops_to_flood = config.flood_op_rate_per_ledger
  * herder.max_tx_set_size()
per_period = ceil(ops_to_flood
  * config.flood_advert_period_ms / ledger_close_ms)
per_period = max(per_period, 1)
→ min(per_period, TX_ADVERT_VECTOR_MAX_SIZE)
```

### Helper: max_demand_size

```
CONST TX_DEMAND_VECTOR_MAX_SIZE = 1000
ledger_close_ms = max(herder.ledger_close_time() * 1000, 1)
ops_to_flood = config.flood_op_rate_per_ledger
  * herder.max_queue_size_ops()
per_period = ceil(ops_to_flood
  * config.flood_demand_period_ms / ledger_close_ms)
per_period = max(per_period, 1)
→ min(per_period, TX_DEMAND_VECTOR_MAX_SIZE)
```

### Helper: retry_delay_demand

```
delay_ms = flood_demand_backoff_delay_ms * attempts
→ min(delay_ms, 2000ms)
```

### clear_tx_advert_history

```
CONST MAX_TX_DEMAND_AGE_SECS = 300
CONST MAX_TX_SET_DONT_HAVE_AGE_SECS = 120

for each peer_adverts in adverts_by_peer:
  adverts.clear_below(ledger_seq)

"Clean up old tx demand history (>5 min)"
cutoff = now - MAX_TX_DEMAND_AGE_SECS
tx_demand_history.retain where last_demanded > cutoff

"Clean up old tx set dont have (>2 min)"
if tx_set_dont_have.len() > 100:
  tx_set_dont_have.clear()

"Clean up old tx set last request (>2 min)"
cutoff_short = now - MAX_TX_SET_DONT_HAVE_AGE_SECS
tx_set_last_request.retain where
  last_request > cutoff_short
```

### record_tx_pull_latency

```
now = current_time()
entry = tx_demand_history[hash]
GUARD entry exists → return

if not entry.latency_recorded:
  entry.latency_recorded = true
  delta = now - entry.first_demanded

if peer in entry.peers:
  delta = now - entry.peers[peer]
```

### Helper: demand_status

```
CONST MAX_RETRY_COUNT = 15

if herder.tx_queue().contains(hash):
  → Discard

entry = tx_demand_history[hash]
if entry not found:
  → Demand

if peer in entry.peers:
  → Discard

num_demanded = entry.peers.len()
if num_demanded < MAX_RETRY_COUNT:
  retry_delay = retry_delay_demand(num_demanded)
  if now - entry.last_demanded >= retry_delay:
    → Demand
  else:
    → RetryLater
else:
  → Discard
```

**Calls**: [retry_delay_demand](#helper-retry_delay_demand)

### Helper: prune_tx_demands

```
CONST MAX_RETRY_COUNT = 15
max_retention = 2s * MAX_RETRY_COUNT * 2

while pending queue not empty:
  hash = pending.front()
  entry = history[hash]
  if entry not found:
    pending.pop_front()
    continue
  if now - entry.first_demanded >= max_retention:
    pending.pop_front()
    history.remove(hash)
  else:
    break  "oldest entry still valid"
```

### run_tx_demands

```
GUARD overlay available → return
peers = overlay.peer_snapshots()
GUARD peers not empty → return

shuffle peers randomly
peer_ids = extract peer_ids
peer_set = set of peer_ids
max_demand_size = max_demand_size()
max_queue_size = max_advert_queue_size()
now = current_time()

adverts_by_peer.retain where peer in peer_set
ensure entry exists for each peer_id

prune_tx_demands(now, pending, history)

"Round-robin demand allocation across peers"
demand_map = { peer → (demand_list, retry_list) }

any_new_demand = true
while any_new_demand:
  any_new_demand = false
  for each peer_id in peer_ids:
    adverts = adverts_by_peer[peer_id]
    (demand, retry) = demand_map[peer_id]
    while demand.len() < max_demand_size
        AND adverts.has_advert():
      hash = adverts.pop_advert()
      status = demand_status(hash, peer_id, now, ...)
      if Demand:
        demand.push(hash)
        update history entry with peer + timestamp
        any_new_demand = true
        break  "one new demand per peer per round"
      if RetryLater:
        retry.push(hash)
      if Discard: skip

"Return retry hashes to peer advert queues"
for each peer_id:
  adverts.retry_incoming(retry, max_queue_size)

"Send demands to peers"
for each (peer_id, hashes) in to_send:
  demand = FloodDemand { tx_hashes: hashes }
  overlay.send_to(peer_id, FloodDemand(demand))
```

**Calls**: [max_demand_size](#helper-max_demand_size) | [max_advert_queue_size](#helper-max_advert_queue_size) | [prune_tx_demands](#helper-prune_tx_demands) | [demand_status](#helper-demand_status)

### handle_flood_advert

```
ledger_seq = min(herder.tracking_slot(), MAX_U32)
max_ops = max_advert_queue_size()
entry = adverts_by_peer[peer_id]
entry.queue_incoming(advert.tx_hashes, ledger_seq,
  max_ops)
```

### handle_flood_demand

```
GUARD overlay available → return

"Use non-blocking send to avoid stalling event loop"
for each hash in demand.tx_hashes:
  tx = herder.tx_queue().get(hash)
  if tx found:
    if overlay.send_to(peer_id, Transaction(tx)):
      sent += 1
    else:
      "Channel full — stop sending"
      break
  else:
    overlay.send_to(peer_id, DontHave(
      type=Transaction, hash))
    if send fails → break
```

### handle_tx_set

"Handle a legacy TxSet message from the network."

```
"Compute hash as SHA-256 of previous_ledger_hash + tx blobs"
transactions = tx_set.txs
prev_hash = tx_set.previous_ledger_hash
hash = compute_non_generalized_hash(
  prev_hash, transactions)     REF: TransactionSet
GUARD hash computed → return

internal_tx_set = TransactionSet.with_hash(
  prev_hash, hash, transactions)
remove hash from tx_set_dont_have
remove hash from tx_set_last_request

if not herder.needs_tx_set(hash):
  NOTE: TxSet not pending (log only)

received_slot = herder.receive_tx_set(internal_tx_set)
if received_slot exists:
  process_externalized_slots()
else if attach_tx_set_by_hash(tx_set)
    OR buffer_externalized_tx_set(tx_set):
  try_apply_buffered_ledgers()
```

**Calls**: [process_externalized_slots](ledger_close.pc.md#process_externalized_slots) | [attach_tx_set_by_hash](ledger_close.pc.md#attach_tx_set_by_hash) | [buffer_externalized_tx_set](ledger_close.pc.md#buffer_externalized_tx_set) | [try_apply_buffered_ledgers](ledger_close.pc.md#try_apply_buffered_ledgers)

### handle_generalized_tx_set

"Handle a GeneralizedTxSet message from the network."

```
"Compute hash as SHA-256 of XDR-encoded GeneralizedTxSet"
xdr_bytes = encode gen_tx_set to XDR
GUARD encoding succeeds → return
hash = SHA256(xdr_bytes)

"Extract transactions from phases"
prev_hash = gen_tx_set.v1.previous_ledger_hash
GUARD gen_tx_set.v1.phases.len() == 2
  → return (invalid phase count)

transactions = flatten all phases:
  V0 phase → components → TxsetCompTxsMaybeDiscountedFee → txs
  V1 phase → execution_stages → clusters → txs

"Validate phase types"
GUARD phases[0] is V0 (classic) → return
GUARD phases[1] is V0 or V1 (soroban) → return

"Validate base fees"
base_fee_limit = current_header.base_fee
for each phase component:
  GUARD component.base_fee >= base_fee_limit
    → return

"Validate tx type segregation"
network_id = self.network_id()
for each tx in transactions:
  frame = TransactionFrame(tx, network_id)
  if frame.is_soroban(): soroban_count++
  else: classic_count++
GUARD classic_count == phases[0] tx count → return
GUARD soroban_count == phases[1] tx count → return

internal_tx_set = TransactionSet.new_generalized(hash, gen_tx_set)
remove hash from tx_set_dont_have
remove hash from tx_set_last_request

received_slot = herder.receive_tx_set(internal_tx_set)
if received_slot exists:
  try_close_slot_directly(slot)
else if attach_tx_set_by_hash(tx_set)
    OR buffer_externalized_tx_set(tx_set):
  try_apply_buffered_ledgers()
```

**Calls**: [try_close_slot_directly](ledger_close.pc.md#try_close_slot_directly) | [attach_tx_set_by_hash](ledger_close.pc.md#attach_tx_set_by_hash) | [buffer_externalized_tx_set](ledger_close.pc.md#buffer_externalized_tx_set) | [try_apply_buffered_ledgers](ledger_close.pc.md#try_apply_buffered_ledgers)

### send_tx_set

"Send a TxSet to a peer in response to GetTxSet."

```
tx_set = herder.get_tx_set(hash)
if tx_set not found:
  "Send DontHave with appropriate message type"
  ledger_version = current_header.ledger_version
  message_type = if ledger_version >= 20:
    GeneralizedTxSet else TxSet
  overlay.send_to(peer_id, DontHave(message_type, hash))
  → return

ledger_version = current_header.ledger_version
if ledger_version >= 20:
  "Try to send as GeneralizedTxSet"
  gen_tx_set = tx_set.generalized_tx_set
    OR build_generalized_tx_set(tx_set)
  if gen_tx_set exists:
    gen_hash = SHA256(encode(gen_tx_set))
    if gen_hash == requested_hash:
      overlay.send_to(peer_id,
        GeneralizedTxSet(gen_tx_set))
      → return

"Fallback: send as legacy TxSet"
xdr_tx_set = TransactionSet {
  previous_ledger_hash: tx_set.prev_hash,
  txs: tx_set.transactions
}
overlay.send_to(peer_id, TxSet(xdr_tx_set))
```

### request_pending_tx_sets

"Request pending transaction sets from peers."

```
CONST TX_SET_REQUEST_TIMEOUT_SECS  // timeout for stale requests

current_ledger = get_current_ledger()
min_slot = current_ledger + 1
window_end = current_ledger + TX_SET_REQUEST_WINDOW

pending = herder.get_pending_tx_sets()
sort pending by slot

"Filter to slots within request window"
pending_hashes = pending
  .filter(slot >= min_slot AND slot <= window_end)
  .take(MAX_TX_SET_REQUESTS_PER_TICK)
GUARD pending_hashes not empty → return

GUARD overlay available → return
peer_infos = overlay.peer_infos()
GUARD peer_infos not empty → return

"Build prioritized peer list"
peers = []
for each peer_info:
  if outbound OR preferred/outbound in DB:
    peers.push(peer_id)
if peers empty: peers = all peers (fallback)
sort peers by bytes

"Determine which hashes to request from which peers"
now = current_time()
clean up dont_have and last_request maps
  to only contain pending hashes

requests = []
for each hash in pending_hashes:
  if not herder.needs_tx_set(hash) → skip

  "Throttle: 200ms between requests for same hash"
  request_state = last_request[hash]
  if elapsed < 200ms → skip

  "Timeout detection: if requesting for
   TX_SET_REQUEST_TIMEOUT_SECS with no response,
   mark all peers as DontHave"
  if request_age >= TX_SET_REQUEST_TIMEOUT_SECS:
    dont_have[hash] = all peers
    tx_set_all_peers_exhausted = true
    continue

  "Find eligible peer (round-robin, skip DontHave)"
  start_idx = tx_set_start_index(hash, peers.len(),
    request_state.next_peer_offset)
  eligible = first peer starting at start_idx
    not in dont_have[hash]

  if no eligible peer:
    "All peers exhausted for this hash"
    tx_set_all_peers_exhausted = true
    continue

  update request_state (time, next_offset)
  requests.push((hash, eligible_peer))

"Warn for newly exhausted tx sets (once per hash)"
for each newly_exhausted:
  tx_set_exhausted_warned.insert(hash)

"Send requests"
for each (hash, peer_id) in requests:
  overlay.send_to(peer_id, GetTxSet(hash))
```

**Calls**: [tx_set_start_index](#helper-tx_set_start_index)

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | ~964   | ~310       |
| Functions     | 20     | 20         |
