## Pseudocode: crates/herder/src/herder.rs

"The Herder is the central coordinator that drives consensus and manages
the transition between ledgers. It integrates with SCP, Overlay, Ledger,
and Transaction processing."

"Operating Modes:
- Observer mode: Tracks consensus by observing EXTERNALIZE messages
- Validator mode: Actively participates in consensus"

"EXTERNALIZE Validation:
1. Quorum membership: Sender must be in our transitive quorum set
2. Slot distance limit: Slot must be within LEDGER_VALIDITY_BRACKET of current"

### Constants

```
CONST MAX_SLOTS_TO_REMEMBER = 12
CONST MAX_TIME_SLIP_SECONDS = 60
CONST MAXIMUM_LEDGER_CLOSETIME_DRIFT = 70
CONST LEDGER_VALIDITY_BRACKET = 100
CONST GENESIS_LEDGER_SEQ = 1
CONST CHECKPOINT_FREQUENCY = 64
CONST DEFAULT_MAX_EXTERNALIZED_SLOTS = 12
```

### Data Structures

```
ENUM EnvelopeState:
  Valid, Pending, Fetching, Duplicate,
  TooOld, InvalidSignature, Invalid

STATE_MACHINE: HerderState
  STATES: [Booting, Syncing, Tracking]
  TRANSITIONS:
    Booting → Syncing: start_syncing()
    Syncing → Tracking: bootstrap() or externalization
    Tracking → Syncing: out-of-sync detected

STRUCT Herder:
  config: HerderConfig
  state: HerderState
  tx_queue: TransactionQueue
  pending_envelopes: PendingEnvelopes
  fetching_envelopes: FetchingEnvelopes
  scp_driver: ScpDriver
  scp: nullable SCP     // only for validators
  tracking_slot: u64
  tracking_consensus_close_time: u64
  tracking_started_at: nullable Instant
  secret_key: nullable SecretKey
  ledger_manager: nullable LedgerManager
  prev_value: Value
  slot_quorum_tracker: SlotQuorumTracker
  quorum_tracker: QuorumTracker
```

### build

```
function build(config, secret_key) -> Herder:
  pending_config = config.pending_config
  max_slots = max(config.max_externalized_slots, 1)
  // pending_config is NOT clamped against max_externalized_slots: the
  // pending buffer's forward capacity and the already-externalized
  // retention window are independent (LEDGER_VALIDITY_BRACKET vs
  // MAX_SLOTS_TO_REMEMBER in stellar-core).

  scp_driver = create ScpDriver with config
  if secret_key exists:
    scp_driver = ScpDriver.with_secret_key(...)
  else:
    scp_driver = ScpDriver.new(...)

  tx_queue = TransactionQueue.new(config.tx_queue_config)
  pending_envelopes = PendingEnvelopes.new(pending_config)
  fetching_envelopes = FetchingEnvelopes.with_defaults()

  "Pre-cache the local quorum set so envelopes referencing it
  don't wait for fetching."
  if config.local_quorum_set exists:
    qs_hash = hash_xdr(quorum_set)
    fetching_envelopes.cache_quorum_set(qs_hash, quorum_set)

  scp = null
  if secret_key exists and config.is_validator:
    if config.local_quorum_set exists:
      scp = SCP.new(node_id, is_validator=true, quorum_set, callback)

  slot_quorum_tracker = SlotQuorumTracker.new(config.local_quorum_set, max_slots)
  quorum_tracker = QuorumTracker.new(node_id)
  if config.local_quorum_set exists:
    quorum_tracker.expand(node_id, quorum_set)

  → Herder { state: Booting, tracking_slot: 0, ... }
```

**Calls:** REF: ScpDriver::new, REF: SCP::new, REF: QuorumTracker::expand

### get_most_recent_checkpoint_seq

```
function get_most_recent_checkpoint_seq() -> u64:
  tracking_consensus_index = tracking_slot - 1
  freq = CHECKPOINT_FREQUENCY
  last = ((tracking_consensus_index / freq) + 1) * freq - 1
  size = if tracking_consensus_index < freq: freq - 1
         else: freq
  → last - (size - 1)
```

### get_min_ledger_seq_to_remember

```
function get_min_ledger_seq_to_remember() -> u64:
  if tracking_slot > MAX_SLOTS_TO_REMEMBER:
    → tracking_slot - MAX_SLOTS_TO_REMEMBER + 1
  else:
    → 1
```

### get_min_ledger_seq_to_ask_peers

```
function get_min_ledger_seq_to_ask_peers() -> u32:
  lcl = ledger_manager.current_ledger_seq() or tracking_slot
  low = lcl + 1
  window = min(max_externalized_slots, 3)
  if low > window:
    low = low - window
  else:
    low = 1
  → low
```

### store_quorum_set

```
function store_quorum_set(node_id, quorum_set):
  scp_driver.store_quorum_set(node_id, quorum_set)
  if not quorum_tracker.expand(node_id, quorum_set):
    quorum_tracker.rebuild(lookup_fn)
```

### out_of_sync_recovery

"Mirrors stellar-core's outOfSyncRecovery(). When out of sync, scan
v-blocking slots from highest to lowest and purge all slots more than
LEDGER_VALIDITY_BRACKET behind the highest v-blocking slot."

```
function out_of_sync_recovery(lcl) -> nullable u64:
  GUARD state == Tracking → null

  v_blocking_slots = get_v_blocking_slots()
  GUARD v_blocking_slots is empty → null

  max_slots_ahead = LEDGER_VALIDITY_BRACKET
  purge_slot = null

  for each slot in v_blocking_slots:
    max_slots_ahead -= 1
    if max_slots_ahead == 0:
      purge_slot = slot
      break

  GUARD purge_slot is null → null

  last_checkpoint = (lcl / 64) * 64
  fetching_envelopes.erase_below(purge_slot, last_checkpoint)
  slot_quorum_tracker.clear_slots_below(purge_slot)
  if scp exists:
    scp.purge_slots(purge_slot - 1)
  scp_driver.purge_slots_below(purge_slot)

  → purge_slot
```

### bootstrap

"Transitions Herder from Syncing to Tracking state."

```
function bootstrap(ledger_seq):
  slot = ledger_seq

  MUTATE tracking_slot = slot
  MUTATE tracking_started_at = now()

  close_time = ledger_manager.current_header().scp_value.close_time
    or 0
  MUTATE tracking_consensus_close_time = close_time

  pending_envelopes.set_current_slot(slot)
  MUTATE state = Tracking
  scp_driver.set_tracking_state(true, slot, close_time)

  "Release any pending envelopes for this slot and previous"
  pending = pending_envelopes.release_up_to(slot)
  for each (pending_slot, envelopes) in pending:
    for each envelope in envelopes:
      process_scp_envelope(envelope)
```

**Calls:** [process_scp_envelope](#process_scp_envelope)

### check_envelope_close_time

"Matches stellar-core HerderImpl::checkCloseTime(SCPEnvelope, enforceRecent).
Called BEFORE signature verification as a cheap pre-filter."

```
function check_envelope_close_time(envelope, enforce_recent) -> bool:
  now = current_unix_time()

  ct_cutoff = if enforce_recent:
    now - MAXIMUM_LEDGER_CLOSETIME_DRIFT
  else: 0

  env_ledger_index = envelope.statement.slot_index
  (lcl_seq, lcl_close_time) = ledger_manager data or (0, 0)

  last_close_index = lcl_seq
  last_close_time = lcl_close_time

  "Use tracking consensus data for a better estimate when available"
  if state != Booting:
    tracking_index = tracking_slot - 1
    if env_ledger_index >= tracking_index
        and tracking_index > last_close_index:
      last_close_index = tracking_index
      last_close_time = tracking_consensus_close_time

  check_value = |value| -> bool:
    sv = decode StellarValue from value
    close_time = sv.close_time

    GUARD close_time < ct_cutoff → false

    "Three cases (any must pass):"
    "1. Exact-match: same slot as last_close_index"
    if last_close_index == env_ledger_index
        and last_close_time == close_time:
      → true
    "2. Older slot"
    if last_close_index > env_ledger_index
        and last_close_time > close_time:
      → true
    "3. Future slot"
    → scp_driver.check_close_time(env_ledger_index,
        last_close_time, close_time)

  "Returns true if ANY value in the envelope passes"
  Nominate:     any(votes + accepted, check_value)
  Prepare:      check ballot.value, prepared, prepared_prime
  Confirm:      check ballot.value
  Externalize:  check commit.value
```

**Calls:** REF: ScpDriver::check_close_time

### receive_scp_envelope

```
function receive_scp_envelope(envelope) -> EnvelopeState:
  state = self.state()
  slot = envelope.statement.slot_index
  current_slot = tracking_slot
  pending_slot = pending_envelopes.current_slot()

  GUARD not state.can_receive_scp() → Invalid

  "**** First perform checks that do NOT require signature verification"

  GUARD not check_envelope_close_time(envelope, false)
    → Invalid

  checkpoint = get_most_recent_checkpoint_seq()
  max_ledger_seq = MAX_INT

  if state.is_tracking():
    max_ledger_seq = next_consensus_ledger_index() + LEDGER_VALIDITY_BRACKET
  else:
    "When not tracking, apply recency-based close-time filtering"
    tracking_consensus_index = current_slot - 1
    enforce_recent = tracking_consensus_index <= GENESIS_LEDGER_SEQ
    if not check_envelope_close_time(envelope, enforce_recent)
        and slot != checkpoint:
      → Invalid

  "Calculate the minimum acceptable slot"
  min_ledger_seq = if current_slot > MAX_SLOTS_TO_REMEMBER:
    current_slot - MAX_SLOTS_TO_REMEMBER + 1
  else: 1

  GUARD (slot > max_ledger_seq or slot < min_ledger_seq)
    and slot != checkpoint → TooOld

  "**** From this point, we have to check signatures"
  GUARD verify_envelope(envelope) fails → InvalidSignature

  slot_quorum_tracker.record_envelope(slot, envelope.node_id)

  "Special handling for EXTERNALIZE messages"
  if envelope is EXTERNALIZE:
    sv = decode StellarValue from ext.commit.value
    tx_set_hash = sv.tx_set_hash

    if lcl is null or slot > lcl:
      scp_driver.request_tx_set(tx_set_hash, slot)

    if slot > current_slot:
      "Security: Validate sender is in our transitive quorum"
      GUARD not quorum_tracker.is_node_definitely_in_quorum(sender)
        → Invalid

      "CRITICAL: Don't externalize without the tx_set!"
      if buffer_envelope_until_tx_set(slot, tx_set_hash, envelope)
          returns a state:
        → that state

      GUARD lcl exists and slot <= lcl → Valid  // already closed

      "Fast-forward to this slot"
      scp_driver.record_externalized(slot, value)
      scp_driver.cleanup_externalized(max_externalized_slots)
      if scp exists: scp.force_externalize(slot, value)
      MUTATE prev_value = value
      advance_tracking_slot(slot)
      → Valid

    else if lcl exists and slot > lcl and slot <= current_slot:
      "Gap slot: between LCL and tracking_slot"
      GUARD not quorum_tracker.is_node_definitely_in_quorum(sender)
        → Invalid

      scp_driver.request_tx_set(tx_set_hash, slot)

      if buffer_envelope_until_tx_set(slot, tx_set_hash, envelope)
          returns a state:
        → that state

      scp_driver.record_externalized(slot, value)
      if scp exists: scp.force_externalize(slot, value)
      → Valid

  "Check if this is for a future slot"
  if slot > current_slot:
    result = pending_envelopes.add(slot, envelope)
    Added     → Pending
    Duplicate → Duplicate
    SlotTooFar → Invalid
    SlotTooOld → process_scp_envelope(envelope_clone)
    BufferFull → Invalid

  "Process envelope for current or recent slot"
  → process_scp_envelope(envelope)
```

**Calls:** [check_envelope_close_time](#check_envelope_close_time), [buffer_envelope_until_tx_set](#buffer_envelope_until_tx_set), [advance_tracking_slot](#advance_tracking_slot), [process_scp_envelope](#process_scp_envelope)

### buffer_envelope_until_tx_set

"Returns Some(state) if buffered/rejected, or null if tx_set is available."

```
function buffer_envelope_until_tx_set(slot, tx_set_hash, envelope)
    -> nullable EnvelopeState:
  if scp_driver.has_tx_set(tx_set_hash):
    → null   // tx_set available, continue

  result = fetching_envelopes.recv_envelope(envelope)
  Ready            → null   // was in fetching cache
  Fetching         → Fetching
  AlreadyProcessed → Duplicate
  Discarded        → Invalid
```

### process_scp_envelope

"Follows stellar-core pattern: only feed envelopes to SCP after their
tx sets are available."

```
function process_scp_envelope(envelope) -> EnvelopeState:
  slot = envelope.statement.slot_index

  tx_set_hashes = get_tx_set_hashes_from_envelope(envelope)
  missing_tx_sets = hashes not in scp_driver cache

  if missing_tx_sets not empty:
    result = fetching_envelopes.recv_envelope(envelope)
    Ready            → process_scp_envelope_with_tx_set(envelope)
    Fetching:
      for each hash in missing_tx_sets:
        scp_driver.request_tx_set(hash, slot)
      → Fetching
    AlreadyProcessed → Duplicate
    Discarded        → Invalid

  → process_scp_envelope_with_tx_set(envelope)
```

**Calls:** [process_scp_envelope_with_tx_set](#process_scp_envelope_with_tx_set)

### process_scp_envelope_with_tx_set

```
function process_scp_envelope_with_tx_set(envelope) -> EnvelopeState:
  slot = envelope.statement.slot_index

  "If we have SCP (validator mode), process through consensus"
  if scp exists:
    result = scp.receive_envelope(envelope)

    if result == Invalid:  → Invalid
    if result == Valid:    → Duplicate   // valid but not new
    if result == ValidNew:
      if heard_from_quorum(slot):
        NOTE: log quorum heard

      if scp.is_slot_externalized(slot):
        value = scp.get_externalized_value(slot)
        scp_driver.record_externalized(slot, value)
        scp_driver.cleanup_externalized(max_externalized_slots)
        MUTATE prev_value = value
        advance_tracking_slot(slot)

      else if envelope is EXTERNALIZE
          and slot == tracking_slot:
        "Validator not in network's quorum — follow network consensus"
        value = ext.commit.value
        scp_driver.record_externalized(slot, value)
        scp_driver.cleanup_externalized(max_externalized_slots)
        scp.force_externalize(slot, value)
        MUTATE prev_value = value
        advance_tracking_slot(slot)

      → Valid

  "Non-validator mode: track externalized values from network"
  if envelope is EXTERNALIZE:
    value = ext.commit.value
    scp_driver.record_externalized(slot, value)
    scp_driver.cleanup_externalized(max_externalized_slots)
    MUTATE prev_value = value
    advance_tracking_slot(slot)

  → Valid
```

**Calls:** [advance_tracking_slot](#advance_tracking_slot)

### advance_tracking_slot

```
function advance_tracking_slot(externalized_slot):
  close_time = scp_driver.get_externalized_close_time(
    externalized_slot) or 0

  if externalized_slot >= tracking_slot:
    MUTATE tracking_slot = externalized_slot + 1
    MUTATE tracking_consensus_close_time = close_time
    pending_envelopes.set_current_slot(externalized_slot + 1)

    "Transition to Tracking on externalization"
    if state != Tracking:
      MUTATE state = Tracking

    scp_driver.set_tracking_state(true, externalized_slot + 1,
      close_time)

    "Release pending envelopes for the new slot"
    pending = pending_envelopes.release(externalized_slot + 1)
    for each env in pending:
      process_scp_envelope(env)
```

### receive_transaction

```
function receive_transaction(tx) -> TxQueueResult:
  GUARD not state.can_receive_transactions()
    → Invalid(null)

  → tx_queue.try_add(tx)
```

### trigger_next_ledger

```
async function trigger_next_ledger(ledger_seq) -> Result:
  GUARD not is_validator() → error NotValidating
  GUARD not is_tracking()  → error NotValidating
  GUARD scp is null        → error NotValidating

  slot = ledger_seq
  previous_hash = ledger_manager.current_header_hash() or ZERO
  starting_seq = build_starting_seq_map(ledger_manager)

  max_txs = ledger_manager.current_header().max_tx_set_size
    or config.max_tx_set_size
  tx_set = tx_queue.get_transaction_set_with_starting_seq(
    previous_hash, max_txs, starting_seq)

  scp_driver.cache_tx_set(tx_set)

  "Parity: clamp to ensure monotonic increase"
  lcl_close_time = ledger_manager.current_header().close_time or 0
  close_time = max(current_unix_time(), lcl_close_time + 1)

  upgrades = encode config.proposed_upgrades to UpgradeType list
  stellar_value = make_stellar_value(tx_set.hash, close_time, upgrades)
  value = encode stellar_value to Value

  prev_value = self.prev_value
  scp.nominate(slot, value, prev_value)
```

**Calls:** [make_stellar_value](#make_stellar_value), [build_starting_seq_map](#helper-build_starting_seq_map)

### make_stellar_value

"Parity: HerderImpl.cpp makeStellarValue — signs with STELLAR_VALUE_SIGNED."

```
function make_stellar_value(tx_set_hash, close_time, upgrades) -> StellarValue:
  sign_data = network_id
    + encode(ENVELOPE_TYPE_SCPVALUE)
    + encode(tx_set_hash)
    + encode(close_time)
  sig = secret_key.sign(sign_data)

  → StellarValue {
      tx_set_hash,
      close_time,
      upgrades,
      ext: Signed(node_id, sig)
    }
```

### Helper: build_starting_seq_map

```
function build_starting_seq_map(manager) -> nullable Map<bytes, i64>:
  snapshot = manager.create_snapshot() or null
  ledger_seq = manager.current_ledger_seq()
  starting_seq = ledger_seq << 32

  map = {}
  for each account in tx_queue.pending_accounts():
    key = account_key_from_account_id(account)
    entry = snapshot.get_account(account)
    if entry exists:
      map[key] = entry.seq_num
    else:
      map[key] = starting_seq

  → map
```

### check_ledger_close

```
function check_ledger_close(slot) -> nullable LedgerCloseInfo:
  externalized = scp_driver.get_externalized(slot) or null
  sv = decode StellarValue from externalized.value

  tx_set_hash = sv.tx_set_hash
  tx_set = scp_driver.get_tx_set(tx_set_hash)

  if tx_set is null:
    scp_driver.request_tx_set(tx_set_hash, slot)

  → LedgerCloseInfo { slot, close_time, tx_set_hash, tx_set,
      upgrades, stellar_value_ext }
```

### ledger_closed

```
function ledger_closed(slot, applied_tx_hashes):
  tx_queue.remove_applied_by_hash(applied_tx_hashes)
  scp_driver.cleanup_old_pending_slots(slot + 1)

  if scp exists:
    scp.purge_slots(slot - 10)

  keep_slot = slot - 2
  fetching_envelopes.erase_below(slot, keep_slot)
  cleanup()
```

**Calls:** [cleanup](#cleanup)

### handle_nomination_timeout

```
function handle_nomination_timeout(slot):
  if scp exists:
    prev_value = self.prev_value
    value = create_nomination_value(slot)
    if value exists:
      scp.nominate_timeout(slot, value, prev_value)
```

### handle_ballot_timeout

```
function handle_ballot_timeout(slot):
  if scp exists:
    scp.bump_ballot(slot)
```

### get_nomination_timeout

```
function get_nomination_timeout(slot) -> nullable Duration:
  if scp exists:
    state = scp.get_slot_state(slot)
    if state.is_nominating:
      → scp.get_nomination_timeout(state.nomination_round)
  → null
```

### get_ballot_timeout

```
function get_ballot_timeout(slot) -> nullable Duration:
  if scp exists:
    state = scp.get_slot_state(slot)
    if state.ballot_round exists
        and state.heard_from_quorum
        and state.ballot_phase != Externalize:
      → scp.get_ballot_timeout(state.ballot_round)
  → null
```

### create_nomination_value

```
function create_nomination_value(slot) -> nullable Value:
  (previous_hash, max_txs, starting_seq) =
    ledger_manager data or (ZERO, config.max_tx_set_size, null)

  (tx_set, gen_tx_set) =
    tx_queue.build_generalized_tx_set_with_starting_seq(
      previous_hash, max_txs, starting_seq)

  scp_driver.cache_tx_set(tx_set)

  close_time = current_unix_time()
  upgrades = encode config.proposed_upgrades
  stellar_value = make_stellar_value(tx_set.hash, close_time, upgrades)
  → encode stellar_value to Value
```

### receive_tx_set

```
function receive_tx_set(tx_set) -> nullable SlotIndex:
  hash = tx_set.hash
  slot = scp_driver.receive_tx_set(tx_set)

  notify_slot = slot or tracking_slot
  fetching_envelopes.tx_set_available(hash, notify_slot)
  process_ready_fetching_envelopes()

  → slot
```

**Calls:** [process_ready_fetching_envelopes](#process_ready_fetching_envelopes)

### process_ready_fetching_envelopes

```
function process_ready_fetching_envelopes() -> int:
  ready_slots = fetching_envelopes.ready_slots()
  processed = 0

  for each slot in ready_slots:
    while envelope = fetching_envelopes.pop(slot):
      process_scp_envelope_with_tx_set(envelope)
      processed += 1

  → processed
```

### cleanup

```
function cleanup():
  scp_driver.cleanup_externalized(max_externalized_slots)
  pending_envelopes.evict_expired()
  tx_queue.evict_expired()
  scp_driver.cleanup_pending_tx_sets(120)
```

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | ~2124  | ~370       |
| Functions     | 62     | 30         |
