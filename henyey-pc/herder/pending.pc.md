## Pseudocode: crates/herder/src/pending.rs

"Pending SCP envelope management."
"Buffers SCP envelopes for future slots; releases them when the slot becomes active."

### Data: PendingConfig

```
CONST MAX_SLOTS        = 100   // matches LEDGER_VALIDITY_BRACKET
CONST MAX_AGE          = 300 seconds

PendingConfig:
  max_slots:         int
  max_age:           Duration

// No per-slot cap — stellar-core does not impose one.
// Removed in #1899 to fix PendingAddBufferFull stall.
```

### Data: PendingEnvelope

```
PendingEnvelope:
  envelope:    ScpEnvelope
  received_at: Timestamp
  hash:        Hash256
```

### Data: PendingEnvelopes

```
PendingEnvelopes:
  config:                     PendingConfig
  slots:                      Map<SlotIndex, List<PendingEnvelope>>
  seen_hashes:                Set<Hash256>
  current_slot:               SlotIndex
  stats:                      PendingStats
  last_buffer_full_warn_slot: u64  // rate-limiting for warn! log
```

### Data: PendingStats

```
PendingStats:
  received:              u64
  added:                 u64
  duplicates:            u64
  too_old:               u64
  released:              u64
  evicted:               u64
  buffer_full:           u64   // total BufferFull rejections
  max_envelopes_per_slot: u64  // high-water mark
```

### Data: PendingResult

```
PendingResult: Added | Duplicate | SlotTooOld | BufferFull
```

### PendingEnvelope::new

```
function new(envelope):
  hash = hash_xdr(envelope)
  → PendingEnvelope {
      envelope, received_at: now(), hash
    }
```

### PendingEnvelope::is_expired

```
function is_expired(max_age):
  → elapsed(self.received_at) > max_age
```

### add

```
function add(slot, envelope):
  stats.received += 1
  current = self.current_slot

  GUARD slot < current        → SlotTooOld
  // No per-slot-distance gate — the envelope-acceptance horizon is
  // enforced ONCE at the pre-filter layer via LEDGER_VALIDITY_BRACKET.
  // See Herder::pre_filter_scp_envelope.

  pending = PendingEnvelope.new(envelope)

  GUARD seen_hashes contains pending.hash
                              → Duplicate

  // Existing slot — just append, no slot-count check.
  if slots contains slot:
    seen_hashes.add(pending.hash)
    slots[slot].append(pending)
    update max_envelopes_per_slot high-water mark
    stats.added += 1
    → Added

  // New slot — enforce max_slots with eviction.
  if slots.count >= config.max_slots:
    evict_old_slots(current)
    GUARD slots.count >= config.max_slots
      stats.buffer_full += 1
                              → BufferFull

  seen_hashes.add(pending.hash)
  slots[slot].append(pending)
  stats.added += 1
  → Added
```

### release

```
function release(slot):
  envelopes = slots.remove(slot)
  if envelopes is null:
    → empty list

  stats.released += envelopes.length

  for each env in envelopes:
    seen_hashes.remove(env.hash)

  → filter envelopes where not is_expired(config.max_age)
    then extract .envelope from each
```

### release_up_to

```
function release_up_to(slot):
  result = ordered map

  slots_to_release = all keys in self.slots where key <= slot

  for each s in slots_to_release:
    envelopes = release(s)
    if envelopes is not empty:
      result[s] = envelopes

  → result
```

### Helper: evict_old_slots

```
function evict_old_slots(current):
  old_slots = all keys in self.slots where key < current

  for each slot in old_slots:
    envelopes = slots.remove(slot)
    stats.evicted += envelopes.length
    for each env in envelopes:
      seen_hashes.remove(env.hash)
```

### purge_slots_below

```
function purge_slots_below(min_slot):
  slots_to_remove = all keys in self.slots where key < min_slot

  for each slot in slots_to_remove:
    envelopes = slots.remove(slot)
    stats.evicted += envelopes.length
    for each env in envelopes:
      seen_hashes.remove(env.hash)
```

### evict_expired

```
function evict_expired():
  for each entry in slots:
    expired_hashes = hashes of envelopes where is_expired(max_age)
    entry.retain(non-expired only)
    removed = initial_len - entry.length

    if removed > 0:
      stats.evicted += removed
      for each hash in expired_hashes:
        seen_hashes.remove(hash)

  slots.retain(entries that are non-empty)
```

### Accessors

```
function len():            → sum of lengths across all slots
function is_empty():       → len() == 0
function slot_count():     → slots.count
function stats():          → self.stats
function has_pending(slot): → slots[slot] exists and is non-empty
function pending_count(slot): → slots[slot].length or 0
function current_slot():   → self.current_slot
function set_current_slot(slot):
  MUTATE self.current_slot = slot
```

### clear

```
function clear():
  slots.clear()
  seen_hashes.clear()
```

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 420    | 100        |
| Functions     | 18     | 15         |
