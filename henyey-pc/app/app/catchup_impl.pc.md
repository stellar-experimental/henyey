## Pseudocode: crates/app/src/app/catchup_impl.rs

STATE_MACHINE: ConsensusStuckAction
  STATES: [Wait, AttemptRecovery, TriggerCatchup]
  TRANSITIONS:
    Wait → AttemptRecovery: recovery timer expired
    AttemptRecovery → TriggerCatchup: recovery attempts exhausted OR stuck timeout
    TriggerCatchup → (terminal): catchup started

---

### catchup

"Run catchup to a target ledger with minimal mode."

```
async function catchup(target):
  → catchup_with_mode(target, CatchupMode.Minimal)
```

---

### catchup_with_mode

"Run catchup to a target ledger with a specific mode."
"Mode controls how much history is downloaded:"
"- Minimal: Only download bucket state at latest checkpoint"
"- Recent(N): Download and replay the last N ledgers"
"- Complete: Download complete history from genesis"

```
async function catchup_with_mode(target, mode):
  set_state(AppState.CatchingUp)
  progress = new CatchupProgress()

  "Phase 1: Determine target ledger"
  if target is Current:
    target_ledger = get_cached_archive_checkpoint()
  else if target is Ledger(seq):
    target_ledger = seq
  else if target is Checkpoint(cp):
    target_ledger = cp * 64

  progress.set_target(target_ledger)

  current = get_current_ledger() or 0

  "Safety: verify the archive has the required checkpoint"
  if target is Ledger(_) AND current > 0:
    target_cp = checkpoint_containing(target_ledger)
    latest_ext = herder.latest_externalized_slot() or 0
    GUARD target_cp > latest_ext → early return (unpublished checkpoint)

  GUARD target_ledger <= current → early return (already at target)
```

**Calls:** [`rebuild_bucket_lists_from_has`](ledger_close.pc.md#rebuild_bucket_lists_from_has)

```
  "Phase 2: Prepare existing bucket state for replay-only catchup (Case 1)"
  if current > GENESIS_LEDGER_SEQ:
    if ledger_manager.is_initialized():
      "Fast path: clone from live ledger manager"
      ledger_manager.resolve_pending_bucket_merges()
      existing_state = ExistingBucketState {
        bucket_list = ledger_manager.bucket_list(),
        hot_archive = ledger_manager.hot_archive_bucket_list(),
        header = ledger_manager.current_header(),
        network_id
      }
      override_lcl = current
    else:
      "Slow path: rebuild from persisted HAS"
      existing_state = rebuild_bucket_lists_from_has()
      override_lcl = current
      NOTE: "Falls back to full catchup on failure"
  else:
    existing_state = null
    override_lcl = null
```

**Calls:** [`run_catchup_work`](#run_catchup_work)

```
  "Phase 3: Run catchup work"
  output = run_catchup_work(target_ledger, mode, progress,
                            existing_state, override_lcl)

  "Phase 4: Persist HAS and LCL to DB after catchup"
  "This is critical: if a second catchup triggers before any ledger close"
  "happens, rebuild_bucket_lists_from_has() would read stale HAS."
  "This matches stellar-core's CatchupWork.cpp."
  final_header = ledger_manager.current_header()
  has = build_history_archive_state(final_header.ledger_seq,
                                     bucket_list, hot_archive, passphrase)
  DB_TRANSACTION:
    store_ledger_header(final_header, header_xdr)
    set_state(HISTORY_ARCHIVE_STATE, has_json)
    set_last_closed_ledger(final_header.ledger_seq)

  "Phase 5: Post-catchup cleanup"
  "Trim buffered ledgers that are now stale"
  buffer.retain(seq > output.result.ledger_seq)

  "Clear bucket manager cache to release memory"
  bucket_manager.clear_cache()

  "Garbage collect stale bucket files (on blocking pool)"
  spawn_blocking:
    ledger_manager.resolve_pending_bucket_merges()
    referenced = ledger_manager.all_referenced_bucket_hashes()
    bucket_manager.retain_buckets(referenced)
    bucket_manager.cleanup_unreferenced_files()

  "Trim herder caches but PRESERVE data for slots > new_lcl"
  herder.trim_scp_driver_caches(new_lcl)
  herder.trim_fetching_caches(new_lcl)
  herder.clear_pending_envelopes()
  tx_set_all_peers_exhausted = false

  "Update cached checkpoint"
  cached_archive_checkpoint = (output.result.ledger_seq, now())
```

**Calls:** [`drain_buffered_ledgers_sync`](ledger_close.pc.md#drain_buffered_ledgers_sync)

```
  "Phase 6: Drain all sequential buffered ledgers before returning"
  "This matches stellar-core's ApplyBufferedLedgersWork."
  drained = drain_buffered_ledgers_sync()

  "Record catchup completion time for cooldown"
  last_catchup_completed_at = now()

  final_ledger = get_current_ledger()
  → CatchupResult { ledger_seq: final_ledger, ledger_hash,
                     buckets_applied, ledgers_replayed }
```

---

### get_cached_archive_checkpoint

"Get the latest checkpoint from archives, with caching."

```
async function get_cached_archive_checkpoint():
  if cache exists AND cache.age < ARCHIVE_CHECKPOINT_CACHE_SECS:
    → cache.checkpoint
  checkpoint = get_latest_checkpoint()
  cache = (checkpoint, now())
  → checkpoint
```

---

### get_latest_checkpoint

```
async function get_latest_checkpoint():
  for each archive in config.history.archives:
    ledger = archive.get_current_ledger()
    if success:
      checkpoint = latest_checkpoint_before_or_at(ledger)
      → checkpoint
  ASSERT: at least one archive succeeded
```

---

### run_catchup_work

```
async function run_catchup_work(target_ledger, mode, progress,
                                 existing_state, override_lcl):
  "Phase 1: Create history archive clients"
  archives = [HistoryArchive.new(url) for url in config.history.archives
              where get_enabled]
  GUARD archives is empty → error

  checkpoint_seq = latest_checkpoint_before_or_at(target_ledger)
  ASSERT: checkpoint_seq exists

  "Only use historywork for Minimal mode WITHOUT existing bucket state"
  if mode == Minimal AND existing_state is null:
    checkpoint_data = download_checkpoint_with_historywork(
      archives.first, checkpoint_seq
    )
  else:
    checkpoint_data = null

  catchup_manager = CatchupManager.new(archives, bucket_manager, db)

  "Determine LCL for mode calculation"
  lcl = override_lcl or get_current_ledger() or GENESIS_LEDGER_SEQ
```

**Calls:** [`CatchupManager::catchup_to_ledger_with_checkpoint_data`](../../catchup/catchup_manager.pc.md#catchup_to_ledger_with_checkpoint_data), [`CatchupManager::catchup_to_ledger_with_mode`](../../catchup/catchup_manager.pc.md#catchup_to_ledger_with_mode)

```
  if checkpoint_data exists:
    output = catchup_manager.catchup_to_ledger_with_checkpoint_data(
      target_ledger, checkpoint_data, ledger_manager
    )
  else:
    output = catchup_manager.catchup_to_ledger_with_mode(
      target_ledger, mode, lcl, existing_state, ledger_manager
    )

  → output
```

---

### download_checkpoint_with_historywork

```
async function download_checkpoint_with_historywork(archive, checkpoint_seq):
  state = new HistoryWorkState()
  scheduler = WorkScheduler.new(max_concurrency: 16)
  builder = HistoryWorkBuilder.new(archive, checkpoint_seq, state, bucket_dir)
  ids = builder.register(scheduler)

  "Run scheduler with progress monitoring"
  scheduler.run_until_done()

  "Verify all work items completed successfully"
  for id in [ids.has, ids.buckets, ids.headers, ids.transactions,
             ids.tx_results, ids.scp_history]:
    ASSERT: scheduler.state(id) == Success

  → build_checkpoint_data(state)
```

---

### start_catchup_message_caching_from_self

"Start caching messages during catchup using the stored weak reference."

```
async function start_catchup_message_caching_from_self():
  app = self_arc.upgrade()
  GUARD app is null → null
  → app.start_catchup_message_caching()
```

---

### start_catchup_message_caching

"Returns a background task handle that caches GeneralizedTxSets"
"and requests tx_sets for EXTERNALIZE messages during catchup."

```
async function start_catchup_message_caching():
  overlay = self.overlay()
  GUARD overlay is null → null
  message_rx = overlay.subscribe_catchup()
  → spawn(cache_messages_during_catchup_impl(message_rx))
```

---

### cache_messages_during_catchup_impl

"Cache messages during catchup to bridge the gap between catchup and live consensus."

```
async function cache_messages_during_catchup_impl(message_rx):
  CONST LEDGER_VALIDITY_BRACKET = 100
  cached_tx_sets = 0
  requested_tx_sets = 0
  recorded_externalized = 0
  rejected_externalized = 0
  requested_hashes = set()

  while msg = message_rx.recv():
    if msg is GeneralizedTxSet(gen_tx_set):
      hash = sha256(xdr_encode(gen_tx_set))
      tx_set = TransactionSet.new_generalized(hash, gen_tx_set)
      herder.cache_tx_set(tx_set)
      cached_tx_sets += 1

    else if msg is ScpMessage(envelope):
      if envelope is EXTERNALIZE:
        slot = envelope.statement.slot_index
        sv = parse StellarValue from envelope

        "Validate close-time"
        lcl_close_time = ledger_manager.current_header().close_time
        GUARD not scp_driver.check_close_time(slot, lcl_close_time, sv.close_time)
          → rejected; continue

        "Validate slot range"
        lcl_seq = ledger_manager.current_ledger_seq()
        GUARD slot > lcl_seq + LEDGER_VALIDITY_BRACKET
          → rejected; continue

        "Verify envelope signature"
        GUARD scp_driver.verify_envelope(envelope) fails
          → rejected; continue

        "All validations passed - record externalized slot"
        scp_driver.record_externalized(slot, value)
        recorded_externalized += 1

        tx_set_hash = sv.tx_set_hash
        if not herder.has_tx_set(tx_set_hash)
           AND tx_set_hash not in requested_hashes:
          scp_driver.request_tx_set(tx_set_hash, slot)
          requested_hashes.add(tx_set_hash)
          "Broadcast GetTxSet to ALL peers"
          overlay.request_tx_set(tx_set_hash)
          requested_tx_sets += 1
```

---

### maybe_start_buffered_catchup

```
async function maybe_start_buffered_catchup():
  CONST EVALUATION_COOLDOWN_SECS = 10

  "Early cooldown check"
  cooldown = last_catchup_completed_at.elapsed()
  GUARD cooldown < EVALUATION_COOLDOWN_SECS → return

  current_ledger = get_current_ledger()

  "Guard: if essentially caught up, do NOT trigger catchup"
  latest_externalized = herder.latest_externalized_slot() or 0
  gap = latest_externalized - current_ledger
  GUARD gap <= TX_SET_REQUEST_WINDOW → return
    NOTE: "Also clears tx_set_all_peers_exhausted if set"

  "Get buffer state after trim"
  trim_syncing_ledgers(buffer, current_ledger)

  "When all peers exhausted, evict consecutive front entries without tx_sets"
  if tx_set_all_peers_exhausted:
    evict consecutive entries from front where tx_set is null

  (first_buffered, last_buffered) = buffer bounds
  GUARD buffer empty → return

  "Check if sequential ledger has tx_set available"
  if first_buffered == current_ledger + 1:
    if buffer[first_buffered].tx_set exists:
      → return  // let try_apply_buffered_ledgers handle it

  "Determine if immediate catchup is possible"
  "stellar-core only triggers when first_buffered is at checkpoint boundary"
  "AND there is at least one more buffered ledger after it."
  can_trigger_immediate =
    is_first_ledger_in_checkpoint(first_buffered)
    AND first_buffered < last_buffered

  if not can_trigger_immediate:
    "Compute required first and trigger ledger"
    if is_first_ledger_in_checkpoint(first_buffered):
      required_first = first_buffered
    else:
      required_first = first_ledger_in_checkpoint(first_buffered)
                       + CHECKPOINT_FREQUENCY
    trigger = required_first + 1

    if last_buffered >= trigger:
      "Proceed to catchup below"
    else:
      "Apply consensus stuck timeout logic"
      stuck_state = consensus_stuck_state

      if stuck_state exists AND stuck_state.current_ledger == current_ledger:
        recently_caught_up = last_catchup_completed_at.elapsed()
                             < POST_CATCHUP_RECOVERY_WINDOW_SECS
        all_peers_exhausted = tx_set_all_peers_exhausted
        has_stale_requests = herder.has_stale_pending_tx_set(
                               TX_SET_UNAVAILABLE_TIMEOUT_SECS)
        recovery_failed = stuck_state.recovery_attempts >= 2

        if recently_caught_up:
          if recovery_attempts >= MAX_POST_CATCHUP_RECOVERY_ATTEMPTS:
            "Recovery is futile — trigger catchup"
            action = TriggerCatchup
          else if since_recovery >= OUT_OF_SYNC_RECOVERY_TIMER_SECS:
            action = AttemptRecovery
          else:
            action = Wait
        else:
          effective_timeout = TX_SET_UNAVAILABLE_TIMEOUT_SECS
                              if (all_peers_exhausted OR has_stale_requests
                                  OR recovery_failed)
                              else CONSENSUS_STUCK_TIMEOUT_SECS
          if catchup_triggered:
            action = Wait
          else if elapsed >= effective_timeout:
            action = TriggerCatchup
          else if since_recovery >= OUT_OF_SYNC_RECOVERY_TIMER_SECS:
            action = AttemptRecovery
          else:
            action = Wait
      else:
        "New stuck state — start recovery timer"
        stuck_state = new ConsensusStuckState(current_ledger,
                                               first_buffered, now())
        action = AttemptRecovery

      if action == Wait: return
      if action == AttemptRecovery:
        out_of_sync_recovery(current_ledger)
        return
      "TriggerCatchup falls through"
```

**Calls:** [`buffered_catchup_target`](#buffered_catchup_target), [`compute_catchup_target_for_timeout`](#compute_catchup_target_for_timeout)

```
  "Determine catchup target"
  target = buffered_catchup_target(current_ledger, first_buffered, last_buffered)
  if target is null:
    target = compute_catchup_target_for_timeout(
      last_buffered, first_buffered, current_ledger
    )
  use_current_target = (target is null)

  GUARD catchup_in_progress already set → return
  GUARD not use_current_target AND (target == 0 OR target <= current_ledger) → return

  "When using CatchupTarget::Current, verify archive has newer checkpoint"
  if use_current_target AND is_checkpoint_ledger(current_ledger):
    latest_cp = get_cached_archive_checkpoint()
    GUARD latest_cp <= current_ledger → return (cooldown applied)

  "Start catchup message caching"
  catchup_message_handle = start_catchup_message_caching_from_self()

  catchup_target = CatchupTarget.Current if use_current_target
                   else CatchupTarget.Ledger(target)
  catchup_result = catchup(catchup_target)

  "Stop message caching"
  if catchup_message_handle: abort it
  catchup_in_progress = false
```

**Calls:** [`handle_catchup_result`](#handle_catchup_result)

```
  handle_catchup_result(catchup_result, reset_stuck_state=true, "Buffered")
```

---

### handle_catchup_result

"Process the result of a catchup operation: update state, bootstrap herder,"
"and apply buffered ledgers. Shared by buffered and externalized catchup paths."

```
async function handle_catchup_result(catchup_result, reset_stuck_state, label):
  if catchup_result is error:
    restore_operational_state()
    last_catchup_completed_at = now()
    if reset_stuck_state:
      "Re-arm recovery cycle for natural backoff"
      stuck_state.catchup_triggered = false
      stuck_state.recovery_attempts = 0
    → return

  result = catchup_result.value
  catchup_did_work = result.buckets_applied > 0 OR result.ledgers_replayed > 0

  if not catchup_did_work:
    restore_operational_state()
    last_catchup_completed_at = now()
    → return

  "Catchup succeeded with actual work"
  if reset_stuck_state:
    consensus_stuck_state = null
  MUTATE self current_ledger = result.ledger_seq
  MUTATE self last_processed_slot = result.ledger_seq
  clear_tx_advert_history(result.ledger_seq)
  herder.bootstrap(result.ledger_seq)
  herder.purge_slots_below(result.ledger_seq)
  herder.cleanup_old_pending_tx_sets(result.ledger_seq + 1)
  reset_tx_set_tracking_after_catchup()

  "Clear stale syncing_ledgers entries above catchup target"
  buffer.retain(seq <= result.ledger_seq)

  restore_operational_state()
  try_apply_buffered_ledgers()

  "Clear stale entries after buffered close"
  buffer.retain(seq <= current_ledger)

  "Reset last_processed_slot to current_ledger so main loop"
  "re-evaluates the gap from current_ledger+1"
  MUTATE self last_processed_slot = current_ledger

  "Reset all tx_set tracking state"
  last_externalized_at = now()
  tx_set_all_peers_exhausted = false
  clear tx_set_dont_have, tx_set_last_request, tx_set_exhausted_warned
  consensus_stuck_state = null

  "Do NOT request SCP state from peers after catchup."
  "That brings in EXTERNALIZE messages for recent slots whose"
  "tx_sets peers have already evicted from their caches."

  last_catchup_completed_at = now()
```

---

### maybe_start_externalized_catchup

```
async function maybe_start_externalized_catchup(latest_externalized):
  CONST CATCHUP_RETRY_COOLDOWN_SECS = 10

  current_ledger = get_current_ledger()
  GUARD latest_externalized <= current_ledger → return

  gap = latest_externalized - current_ledger
  GUARD gap <= TX_SET_REQUEST_WINDOW → return

  "Cooldown: don't retry immediately after a catchup attempt"
  GUARD last_catchup_completed_at.elapsed() < CATCHUP_RETRY_COOLDOWN_SECS → return

  GUARD catchup_in_progress already set → return

  target = latest_externalized - TX_SET_REQUEST_WINDOW
  GUARD target == 0 OR target <= current_ledger → return

  "Skip when target checkpoint hasn't been published yet"
  target_checkpoint = checkpoint_containing(target)
  GUARD target_checkpoint > latest_externalized → return (cooldown applied)

  "Start catchup message caching"
  catchup_message_handle = start_catchup_message_caching_from_self()

  catchup_result = catchup(CatchupTarget.Ledger(target))

  "Stop message caching"
  if catchup_message_handle: abort it
  catchup_in_progress = false

  handle_catchup_result(catchup_result, reset_stuck_state=false, "Externalized")
```

---

### buffered_catchup_target

```
function buffered_catchup_target(current_ledger, first_buffered, last_buffered):
  GUARD first_buffered <= current_ledger + 1 → null

  gap = first_buffered - current_ledger
  if gap >= CHECKPOINT_FREQUENCY:
    "Target the latest checkpoint before first_buffered"
    target = latest_checkpoint_before_or_at(first_buffered - 1)
    → target if > 0, else null

  required_first = if is_first_ledger_in_checkpoint(first_buffered):
    first_buffered
  else:
    first_ledger_in_checkpoint(first_buffered) + CHECKPOINT_FREQUENCY

  trigger = required_first + 1
  GUARD last_buffered < trigger → null

  target = required_first - 1
  → target if > 0, else null
```

---

### compute_catchup_target_for_timeout

"Compute a catchup target when stuck waiting for buffered ledgers."
"Returns null if no published checkpoint is ahead of current_ledger."

```
function compute_catchup_target_for_timeout(last_buffered, first_buffered,
                                             current_ledger):
  first_cp_start = first_ledger_in_checkpoint(first_buffered)

  "Target = last ledger of checkpoint BEFORE first_buffered's checkpoint"
  target = first_cp_start - 1  (or first_buffered - 1 if first_cp_start == 0)

  if target <= current_ledger:
    "Try last_buffered's checkpoint instead"
    alt_target = first_ledger_in_checkpoint(last_buffered) - 1
    if alt_target > current_ledger:
      → alt_target

    "Tiny gap: target first_buffered - 1 directly (Case 1 replay)"
    direct_target = first_buffered - 1
    if direct_target > current_ledger:
      → direct_target

    → null  // caller falls through to CatchupTarget::Current

  → target
```

---

### reset_tx_set_tracking_after_catchup

"Reset tx_set tracking after catchup to give pending tx_sets a fresh chance."

```
async function reset_tx_set_tracking_after_catchup():
  clear tx_set_dont_have
  clear tx_set_last_request
```

---

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | ~1400  | ~400       |
| Functions     | 14     | 14         |
