## Pseudocode: crates/herder/src/tx_queue/tx_set.rs

"Transaction set construction, hashing, validation, and serialization."

```
CONST MAX_RESOURCE_FEE = 2^50   // maximum Soroban resource fee
```

### Data: TransactionSet

```
TransactionSet:
  hash:                 Hash256
  previous_ledger_hash: Hash256
  transactions:         List<TransactionEnvelope>
  generalized_tx_set:   GeneralizedTransactionSet or null
```

### Helper: sort_txs_by_hash

```
function sort_txs_by_hash(txs):
  sort txs by hash_xdr(tx) ascending
```

### compute_non_generalized_hash

"Legacy TransactionSet hash: SHA-256 of (previous_ledger_hash ++ each tx XDR)."

```
function compute_non_generalized_hash(
    previous_ledger_hash, transactions):
  hasher = SHA256.new()
  hasher.update(previous_ledger_hash)
  for each tx in transactions:
    bytes = xdr_encode(tx)
    hasher.update(bytes)
  → hasher.finalize()
```

### new

```
function new(previous_ledger_hash, transactions):
  sort_txs_by_hash(transactions)
  hash = compute_non_generalized_hash(
    previous_ledger_hash, transactions)
  → TransactionSet {
      hash, previous_ledger_hash, transactions,
      generalized_tx_set: null
    }
```

### with_hash

```
function with_hash(previous_ledger_hash, hash, transactions):
  → TransactionSet {
      hash, previous_ledger_hash, transactions,
      generalized_tx_set: null
    }
```

### new_generalized

```
function new_generalized(hash, gen_tx_set):
  → TransactionSet {
      hash,
      body: Generalized(gen_tx_set)
    }
```

### recompute_hash

```
function recompute_hash():
  if generalized_tx_set is set:
    bytes = xdr_encode(generalized_tx_set)
    → SHA256(bytes)
  → compute_non_generalized_hash(
      previous_ledger_hash, transactions)
```

### summary

```
function summary():
  if transactions is empty:
    → "empty tx set"

  if generalized_tx_set is set:
    → summary_generalized_tx_set(generalized_tx_set)

  tx_count = transactions.length
  op_count = sum(tx_operation_count(tx) for each tx)
  base_fee = min(tx_inclusion_fee(tx) / ops
                 for each tx where ops > 0)
  → "txs:{tx_count}, ops:{op_count}, base_fee:{base_fee}"
```

### to_xdr_stored_set

```
function to_xdr_stored_set():
  if generalized_tx_set is set:
    → StoredTransactionSet.V1(generalized_tx_set)
  else:
    legacy = TransactionSet {
      previous_ledger_hash, txs: transactions
    }
    → StoredTransactionSet.V0(legacy)
```

### from_xdr_stored_set

```
function from_xdr_stored_set(stored):
  if stored is V0(legacy):
    prev_hash = legacy.previous_ledger_hash
    transactions = legacy.txs
    hash = compute_non_generalized_hash(
      prev_hash, transactions)
    GUARD hash is null → error
    → TransactionSet { hash, prev_hash, transactions,
                        generalized_tx_set: null }

  if stored is V1(gen):
    prev_hash = gen.previous_ledger_hash
    transactions = extract_transactions_from_generalized(gen)
    hash = SHA256(xdr_encode(gen))
    → TransactionSet { hash, prev_hash, transactions,
                        generalized_tx_set: gen }
```

### prepare_for_apply

"Corresponds to upstream TxSetXDRFrame::prepareForApply()."

```
function prepare_for_apply(network_id):
  if generalized_tx_set is set:
    → prepare_generalized_for_apply(
        generalized_tx_set, network_id)
  else:
    → prepare_legacy_for_apply(
        previous_ledger_hash, transactions, network_id)
```

### Helper: prepare_generalized_for_apply

```
function prepare_generalized_for_apply(gen, network_id):
  validate_generalized_tx_set_xdr_structure(gen)

  all_transactions = []
  for each (phase_id, phase) in gen.phases:
    expect_soroban = (phase_id == 1)

    if phase is V0(components):
      for each component in components:
        validate_wire_txs(
          component.txs, network_id, expect_soroban)
        all_transactions.extend(component.txs)

    if phase is V1(parallel):
      for each stage in parallel.execution_stages:
        for each cluster in stage:
          validate_wire_txs(
            cluster, network_id, expect_soroban)
          all_transactions.extend(cluster)

  hash = SHA256(xdr_encode(gen))
  prev_hash = gen.previous_ledger_hash

  → TransactionSet { hash, prev_hash,
      all_transactions, generalized_tx_set: gen }
```

### Helper: prepare_legacy_for_apply

```
function prepare_legacy_for_apply(prev_hash, txs, network_id):
  for each env in txs:
    validate_tx_fee(env)
    frame = TransactionFrame(env, network_id)
    GUARD frame.is_soroban()
      → "Legacy tx set contains Soroban transaction"

  GUARD not is_sorted_by_hash(txs)
    → "Transactions are not sorted correctly"

  hash = compute_non_generalized_hash(prev_hash, txs)
  → TransactionSet { hash, prev_hash, txs,
      generalized_tx_set: null }
```

### Helper: validate_generalized_tx_set_xdr_structure

"Mirrors upstream validateTxSetXDRStructure."

```
function validate_generalized_tx_set_xdr_structure(gen):
  GUARD gen.phases.length != 2
    → "Expected exactly 2 phases"

  for each (phase_id, phase) in gen.phases:
    if phase is V0(components):
      validate_sequential_phase_xdr_structure(components)
    if phase is V1(parallel):
      GUARD phase_id != 1
        → "Non-Soroban parallel phase"
      validate_parallel_component(parallel)
```

### Helper: validate_sequential_phase_xdr_structure

```
function validate_sequential_phase_xdr_structure(components):
  "Components must be sorted by base_fee ascending"
  "None < Some, no duplicates"
  GUARD not sorted → "Incorrect component order"

  for each component in components:
    GUARD component.txs is empty
      → "Empty component in sequential phase"
```

### Helper: validate_parallel_component

```
function validate_parallel_component(parallel):
  for each stage in parallel.execution_stages:
    GUARD stage is empty → "Empty stage"
    for each cluster in stage:
      GUARD cluster is empty → "Empty cluster"
```

### Helper: validate_tx_fee

"Mirrors upstream XDRProvidesValidFee."

```
function validate_tx_fee(env):
  is_soroban = any operation in env is
    InvokeHostFunction or ExtendFootprintTtl
    or RestoreFootprint

  if is_soroban:
    GUARD env is TxV0
      → "Soroban uses TxV0 envelope"
    GUARD env has no SorobanTransactionData
      → "Missing SorobanTransactionData"
    resource_fee = soroban_data.resource_fee
    GUARD resource_fee < 0 or resource_fee > MAX_RESOURCE_FEE
      → "Resource fee out of valid range"
```

### Helper: validate_wire_txs

```
function validate_wire_txs(txs, network_id, expect_soroban):
  for each env in txs:
    validate_tx_fee(env)
    frame = TransactionFrame(env, network_id)
    GUARD frame.is_soroban() != expect_soroban
      → "Wrong tx type in phase"

  GUARD not is_sorted_by_hash(txs)
    → "Transactions not sorted within component"
```

### Helper: is_sorted_by_hash

```
function is_sorted_by_hash(txs):
  → all consecutive pairs satisfy
    hash_xdr(a) <= hash_xdr(b)
```

### Helper: extract_transactions_from_generalized

```
function extract_transactions_from_generalized(gen):
  transactions = []
  for each phase in gen.phases:
    if phase is V0(components):
      for each component in components:
        transactions.extend(component.txs)
    if phase is V1(parallel):
      for each stage in parallel.execution_stages:
        for each cluster in stage:
          transactions.extend(cluster)
  → transactions
```

### Helper: tx_operation_count

```
function tx_operation_count(envelope):
  → envelope.operations.length
  NOTE: For fee-bump, uses inner tx operations
```

### Helper: tx_inclusion_fee

```
function tx_inclusion_fee(envelope):
  → envelope.fee
  NOTE: For fee-bump, uses outer fee
```

### Helper: summary_generalized_tx_set

```
function summary_generalized_tx_set(gen):
  parts = []
  for each (phase_idx, phase) in gen.phases:
    component_stats = ordered map<base_fee, (tx_count, op_count)>

    if phase is V0(components):
      for each component in components:
        for each tx in component.txs:
          stats[component.base_fee].tx_count += 1
          stats[component.base_fee].op_count +=
            tx_operation_count(tx)

    if phase is V1(parallel):
      for each stage in parallel.execution_stages:
        for each cluster in stage:
          for each tx in cluster:
            stats[parallel.base_fee].tx_count += 1
            stats[parallel.base_fee].op_count +=
              tx_operation_count(tx)

    phase_name = "classic" if phase_idx==0
                 else "soroban" if phase_idx==1
    parts.append("{phase_name} phase: ... [{stats}]")

  → join(parts, ", ")
```

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 633    | 200        |
| Functions     | 17     | 17         |
