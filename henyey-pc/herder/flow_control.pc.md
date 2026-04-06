## Pseudocode: crates/herder/src/flow_control.rs

"Flow control constants and helpers for transaction size limits."
"Matches stellar-core Herder.h and Peer.h."

CONST MAX_CLASSIC_TX_SIZE_BYTES      = 102400   // 100 KB
CONST FLOW_CONTROL_BYTES_EXTRA_BUFFER = 2000    // envelope overhead
CONST SOROBAN_PROTOCOL_VERSION        = 20

### compute_max_tx_size

```
function compute_max_tx_size(protocol_version,
                              soroban_tx_max_size_bytes):
  if protocol_version >= SOROBAN_PROTOCOL_VERSION:
    if soroban_tx_max_size_bytes is set:
      soroban_limit = soroban_max + EXTRA_BUFFER
      → max(MAX_CLASSIC_TX_SIZE_BYTES, soroban_limit)

  → MAX_CLASSIC_TX_SIZE_BYTES
```

### is_tx_too_large

```
function is_tx_too_large(tx_size, max_tx_size):
  → tx_size > max_tx_size
```

### compute_reading_capacity

```
function compute_reading_capacity(configured_capacity,
                                   max_tx_size, multiplier):
  if configured_capacity > 0:
    → configured_capacity
  → max_tx_size * multiplier     // default multiplier = 300
```

### compute_send_more_batch_size

```
function compute_send_more_batch_size(configured_batch_size,
                                       max_tx_size, batch_count):
  if configured_batch_size > 0:
    → configured_batch_size
  → max_tx_size * batch_count    // default batch_count = 40
```

### FlowControlConfig

```
STRUCT FlowControlConfig:
  max_tx_size           "max transaction size (bytes)"
  max_classic_tx_size   "max classic tx size (fixed)"
  extra_buffer          "Soroban extra buffer"
  reading_capacity      "peer reading capacity (bytes)"
  send_more_batch_size  "send-more batch size (bytes)"
```

### FlowControlConfig::new

```
function FlowControlConfig.new(protocol_version,
                                soroban_tx_max_size_bytes,
                                configured_reading_capacity,
                                configured_batch_size):
  max_tx_size = compute_max_tx_size(protocol_version,
                                     soroban_tx_max_size_bytes)
  reading_capacity = compute_reading_capacity(
    configured_reading_capacity, max_tx_size, 300)
  send_more_batch_size = compute_send_more_batch_size(
    configured_batch_size, max_tx_size, 40)
  → FlowControlConfig { ... }
```

**Calls**: [compute_max_tx_size](#compute_max_tx_size) | [compute_reading_capacity](#compute_reading_capacity) | [compute_send_more_batch_size](#compute_send_more_batch_size)

### update_for_soroban

```
function update_for_soroban(new_tx_max_size_bytes):
  new_max = new_tx_max_size_bytes + extra_buffer
  new_max = max(max_classic_tx_size, new_max)

  diff = 0
  if new_max > self.max_tx_size:
    diff = new_max - self.max_tx_size

  // Always update, even on decrease (matches stellar-core)
  MUTATE self.max_tx_size = new_max
  → diff
```

### is_tx_size_valid

```
function is_tx_size_valid(tx_size):
  → tx_size <= self.max_tx_size
```

## Summary

| Metric       | Source | Pseudocode |
|--------------|--------|------------|
| Lines (logic)| 110    | 52         |
| Functions    | 7      | 7          |
