## Pseudocode: crates/herder/src/tx_queue/selection.rs

"Transaction selection for consensus nomination."
"Selects highest-fee transactions respecting per-lane resource limits."

### get_transaction_set

```
function get_transaction_set(previous_ledger_hash, max_ops):
  selected = select_transactions_with_starting_seq(
    max_ops, null
  )
  → TransactionSet.new(previous_ledger_hash, selected.transactions)
```

### get_transaction_set_with_starting_seq

```
function get_transaction_set_with_starting_seq(
    previous_ledger_hash, max_ops, starting_seq):
  selected = select_transactions_with_starting_seq(
    max_ops, starting_seq
  )
  → TransactionSet.new(previous_ledger_hash, selected.transactions)
```

### build_generalized_tx_set_with_starting_seq

"Build GeneralizedTransactionSet (protocol 20+) with correct hash."

```
function build_generalized_tx_set_with_starting_seq(
    previous_ledger_hash, max_ops, starting_seq):

  selected = select_transactions_with_starting_seq(
    max_ops, starting_seq
  )
  base_fee = validation_context.base_fee

  // --- Phase 1: Partition into classic vs soroban ---
  classic_txs = []
  soroban_txs = []
  for each tx in selected.transactions:
    frame = TransactionFrame(tx, network_id)
    if frame.is_soroban():
      soroban_txs.append(tx)
    else:
      classic_txs.append(tx)

  sort_txs_by_hash(soroban_txs)

  // --- Phase 2: Build classic phase components ---
  classic_components = []
  if classic_txs is not empty:
    has_dex_lane = config.max_dex_ops is set
    lane_count = 2 if has_dex_lane else 1
    lowest_lane_fee = [MAX_INT] * lane_count
    lane_for_tx = []

    for each tx in classic_txs:
      frame = TransactionFrame(tx, network_id)
      lane = DEX_LANE if has_dex_lane and frame.has_dex_operations()
             else GENERIC_LANE
      per_op_fee = envelope_fee_per_op(tx)
      if per_op_fee < lowest_lane_fee[lane]:
        lowest_lane_fee[lane] = per_op_fee
      lane_for_tx.append(lane)

    min_lane_fee = min(lowest_lane_fee) or base_fee
    lane_base_fee = [base_fee] * lane_count
    if selected.classic_limited:
      lane_base_fee.fill(min_lane_fee)
    if has_dex_lane and selected.dex_limited:
      lane_base_fee[DEX_LANE] = lowest_lane_fee[DEX_LANE]

    "Group classic txs by their effective lane base fee"
    components_by_fee = ordered map
    for each (tx, lane) in zip(classic_txs, lane_for_tx):
      fee = lane_base_fee[lane]
      components_by_fee[fee].append(tx)

    for each (fee, txs) in components_by_fee:
      sort_txs_by_hash(txs)
      classic_components.append(
        TxSetComponentDiscountedFee(base_fee=fee, txs)
      )

  classic_phase = TransactionPhase.V0(classic_components)

  // --- Phase 3: Build soroban phase ---
  soroban_base_fee =
    if selected.soroban_limited:
      min(fee_per_op for each in soroban_txs) or base_fee
    else if soroban_txs is empty:
      null
    else:
      base_fee

  use_parallel_builder =
    soroban_txs is not empty
    and config.ledger_max_instructions > 0
    and config.ledger_max_dependent_tx_clusters > 0
    and config.soroban_phase_max_stage_count > 0

  if soroban_txs is empty:
    soroban_phase = ParallelTxsComponent(
      base_fee=soroban_base_fee, stages=[])
  else if use_parallel_builder:
    stages = build_parallel_soroban_phase(
      soroban_txs, network_id,
      config.ledger_max_instructions,
      config.ledger_max_dependent_tx_clusters,
      config.soroban_phase_min_stage_count,
      config.soroban_phase_max_stage_count)
    soroban_phase = stages_to_xdr_phase(
      stages, soroban_base_fee)
  else:
    "Single-stage fallback: one cluster with all soroban txs"
    cluster = DependentTxCluster(soroban_txs)
    stage = ParallelTxExecutionStage([cluster])
    soroban_phase = ParallelTxsComponent(
      base_fee=soroban_base_fee, stages=[stage])

  // --- Phase 4: Assemble and hash ---
  gen_tx_set = GeneralizedTransactionSet.V1 {
    previous_ledger_hash,
    phases: [classic_phase, soroban_phase]
  }

  hash = SHA256(xdr_encode(gen_tx_set))

  tx_set = TransactionSet.new_generalized(hash, gen_tx_set)
  → (tx_set, gen_tx_set)
```

**Calls:** [`build_parallel_soroban_phase`](../parallel_tx_set_builder.pc.md#build_parallel_soroban_phase), [`stages_to_xdr_phase`](../parallel_tx_set_builder.pc.md#stages_to_xdr_phase), [`TransactionSet::new`](tx_set.pc.md#new), [`TransactionSet::new_generalized`](tx_set.pc.md#new_generalized)

### select_transactions_with_starting_seq

"Core selection algorithm: dedup, sequence continuity, surge pricing."

```
function select_transactions_with_starting_seq(
    max_ops, starting_seq):

  // --- Step 1: Group non-expired txs by account ---
  per_account = map<account_key, list<QueuedTransaction>>
  for each tx in by_hash.values():
    if tx.is_expired(config.max_age_secs):
      continue
    per_account[account_key(tx)].append(tx)

  // --- Step 2: Dedup + sequence continuity ---
  layered = map<account_key, list<QueuedTransaction>>
  for each (account, txs) in per_account:
    sort txs by (sequence_number ASC,
                  fee_rate DESC,
                  hash ASC)

    "Keep best fee per sequence number"
    deduped = map<seq, QueuedTransaction>
    for each tx in txs:
      seq = tx.sequence_number()
      if seq not in deduped:
        deduped[seq] = tx
      else if better_fee_ratio(tx, deduped[seq]):
        deduped[seq] = tx

    "Build contiguous sequence chain"
    seqs = sorted keys of deduped
    contiguous = []
    expected = starting_seq[account] + 1 if provided
    for each seq in seqs:
      if expected is set:
        if seq < expected: continue
        if seq != expected: break
      contiguous.append(deduped[seq])
      expected = seq + 1

    if contiguous is not empty:
      layered[account] = contiguous

  // --- Step 3: Split classic vs soroban ---
  classic_accounts = map
  soroban_accounts = map
  for each (account, txs) in layered:
    seen_soroban = false
    for each tx in txs:
      is_soroban = TransactionFrame(tx).is_soroban()
      if not seen_soroban:
        if is_soroban:
          seen_soroban = true
          soroban_accounts[account].append(tx)
        else:
          classic_accounts[account].append(tx)
      else:
        if not is_soroban: break
        soroban_accounts[account].append(tx)

  // --- Step 4: Surge-price classic txs ---
  classic_limit = Resource([max_ops, classic_bytes])
  dex_limit = config.max_dex_ops (if set)
  lane_config = DexLimitingLaneConfig(
    classic_limit, dex_limit)
  classic_queue = SurgePricingPriorityQueue(lane_config)

  "Seed each account's first tx into the priority queue"
  for each (account, txs) in classic_accounts:
    classic_queue.add(txs[0])

  classic_selected = []
  classic_lane_left = per-lane resource limits
  had_not_fitting = [false] * lane_count

  while classic_queue has top entry (lane, entry):
    resources = classic_queue.tx_resources(entry)
    exceeds_lane = resources > lane_left[lane]
    exceeds_generic = resources > lane_left[GENERIC_LANE]

    if exceeds_lane or exceeds_generic:
      had_not_fitting[offending lane] = true
      classic_queue.remove_entry(lane, entry)
      continue

    classic_selected.append(entry.tx)
    lane_left[GENERIC_LANE] -= resources
    if lane != GENERIC_LANE:
      lane_left[lane] -= resources

    classic_queue.remove_entry(lane, entry)

    "Feed next tx from same account"
    next_index = position[account] + 1
    if next_index < account_txs.length:
      classic_queue.add(account_txs[next_index])

  classic_limited = had_not_fitting[GENERIC_LANE]
  dex_limited = had_not_fitting[DEX_LANE]

  // --- Step 5: Surge-price soroban txs ---
  soroban_limit = config.max_soroban_resources
    or derived from config.max_soroban_bytes
  if soroban_limit exists:
    soroban_queue = SurgePricingPriorityQueue(
      SorobanGenericLaneConfig(soroban_limit))

    "Same feed-next-from-account loop as classic"
    soroban_selected, soroban_limited =
      run_surge_pricing_loop(soroban_queue,
        soroban_accounts)
  else:
    "No limit — take all soroban txs"
    soroban_selected = all txs from soroban_accounts
    soroban_limited = false

  // --- Step 6: Combine ---
  transactions = classic_selected ++ soroban_selected

  → SelectedTxs {
      transactions,
      soroban_limited,
      dex_limited,
      classic_limited
    }
```

**Calls:** [`SurgePricingPriorityQueue::add`](../surge_pricing.pc.md#add), [`SurgePricingPriorityQueue::peek_top`](../surge_pricing.pc.md#peek_top), [`SurgePricingPriorityQueue::remove_entry`](../surge_pricing.pc.md#remove_entry)

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 491    | 155        |
| Functions     | 5      | 4          |
