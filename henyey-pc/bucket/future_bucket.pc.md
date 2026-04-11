## Pseudocode: crates/bucket/src/future_bucket.rs

"FutureBucket — Async bucket merging support."
"Enables background merge execution, serialization to HistoryArchiveState,"
"and restarting merges after deserialization."

STATE_MACHINE: FutureBucket
  STATES: [Clear, HashOutput, HashInputs, LiveOutput, LiveInputs]
  TRANSITIONS:
    Clear → LiveInputs: start_merge() with live buckets
    LiveInputs → LiveOutput: resolve() when merge completes
    LiveOutput → HashOutput: to_snapshot() for serialization
    LiveInputs → HashInputs: to_snapshot() for serialization
    HashOutput → LiveOutput: make_live() loads output bucket
    HashInputs → LiveInputs: make_live() loads inputs + restarts merge

### FutureBucketState (enum)

```
ENUM FutureBucketState:
  Clear       // no inputs, no outputs, no hashes
  HashOutput  // output hash present, no live bucket
  HashInputs  // input hashes present, no live buckets
  LiveOutput  // live output bucket available
  LiveInputs  // live inputs, merge in progress
```

### MergeKey (struct)

```
STRUCT MergeKey:
  keep_tombstones: boolean
  curr_hash: Hash256
  snap_hash: Hash256
```

### FutureBucketSnapshot (struct)

```
STRUCT FutureBucketSnapshot:
  state: FutureBucketState
  curr: optional string    // curr hash hex
  snap: optional string    // snap hash hex
  output: optional string  // output hash hex
```

### FutureBucket (struct)

```
STRUCT FutureBucket:
  state: FutureBucketState
  input_curr: optional shared Bucket
  input_snap: optional shared Bucket
  output: optional shared Bucket
  merge_handle: optional async handle
  input_curr_hash: optional Hash256
  input_snap_hash: optional Hash256
  output_hash: optional Hash256
  protocol_version: integer
  keep_tombstones: boolean
  normalize_init: boolean
```

### clear

```
function clear():
  → FutureBucket(state=Clear, all fields empty)
```

### start_merge

"Immediately starts the merge in a background task."

```
function start_merge(curr, snap, protocol_version,
                     keep_tombstones, normalize_init):
  curr_hash = curr.hash()
  snap_hash = snap.hash()

  spawn background task:
    result = merge_buckets(
      curr, snap, keep_tombstones,
      protocol_version, normalize_init)
    send result to channel

  → FutureBucket(
      state = LiveInputs,
      input_curr = curr, input_snap = snap,
      merge_handle = channel receiver,
      curr_hash, snap_hash)
```

**Calls**: [merge_buckets](merge.pc.md#merge_buckets)

### from_output

```
function from_output(bucket):
  hash = bucket.hash()
  → FutureBucket(state = LiveOutput,
                 output = bucket, output_hash = hash)
```

### from_snapshot

"Deserialize from snapshot — only Clear, HashOutput, HashInputs valid."

```
function from_snapshot(snapshot):
  if Clear:      → clear()
  if HashOutput:
    GUARD missing output hash → error
    parse output_hash from hex
    → FutureBucket(state=HashOutput, output_hash=hash)
  if HashInputs:
    GUARD missing curr or snap hash → error
    parse curr_hash, snap_hash from hex
    → FutureBucket(state=HashInputs,
                   curr_hash, snap_hash)
  otherwise: → error "invalid deserialized state"
```

### State queries

```
function state():          → self.state
function is_live():        → LiveInputs or LiveOutput
function is_merging():     → LiveInputs
function is_clear():       → Clear
function has_hashes():     → HashInputs or HashOutput
function merge_complete(): → true if LiveOutput
function is_ready():
  if LiveOutput: → true
  if merge_handle exists: → handle.is_complete()
  → false
```

### resolve (async)

"Wait for merge to complete; transitions to LiveOutput."

```
function resolve():
  if LiveOutput:
    → output bucket
  if LiveInputs:
    GUARD no merge handle → error "already consumed"
    bucket = await merge_handle.resolve()
    clear input_curr, input_snap, input hashes
    set output = bucket
    set output_hash = bucket.hash()
    state = LiveOutput
    → bucket
  otherwise: → error "cannot resolve in this state"
```

### resolve_blocking

"Synchronous merge — performs merge inline without async."

```
function resolve_blocking():
  if LiveOutput:
    → output bucket
  if LiveInputs:
    GUARD missing curr or snap → error
    bucket = merge_buckets(
      curr, snap, keep_tombstones,
      protocol_version, normalize_init)
    clear inputs and merge_handle
    set output = bucket
    set output_hash = bucket.hash()
    state = LiveOutput
    → bucket
  otherwise: → error "cannot resolve in this state"
```

**Calls**: [merge_buckets](merge.pc.md#merge_buckets)

### to_snapshot

```
function to_snapshot():
  Clear:
    → default snapshot (Clear state)
  HashOutput or LiveOutput:
    → snapshot(state=HashOutput,
               output=output_hash hex)
  HashInputs or LiveInputs:
    → snapshot(state=HashInputs,
               curr=curr_hash hex,
               snap=snap_hash hex)
```

### make_live

"Load buckets from disk and restart merge after deserialization."

```
function make_live(load_bucket, protocol_version,
                   keep_tombstones, normalize_init):
  if HashOutput:
    GUARD missing output hash → error
    bucket = load_bucket(output_hash)
    output = bucket
    state = LiveOutput

  if HashInputs:
    GUARD missing curr or snap hash → error
    curr = load_bucket(curr_hash)
    snap = load_bucket(snap_hash)

    "Restart the merge in background task"
    spawn background task:
      result = merge_buckets(
        curr, snap, keep_tombstones,
        protocol_version, normalize_init)
      send result to channel

    input_curr = curr, input_snap = snap
    merge_handle = channel receiver
    state = LiveInputs

  if Clear: → nothing (no-op)
  otherwise: → error "cannot make live in this state"
```

**Calls**: [merge_buckets](merge.pc.md#merge_buckets)

### get_hashes / merge_key

```
function get_hashes():
  collect all non-empty hashes from
    input_curr_hash, input_snap_hash, output_hash
  → list of hashes

function merge_key():
  if LiveInputs or HashInputs:
    → MergeKey(keep_tombstones, curr_hash, snap_hash)
  → nothing
```

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | 596    | 155        |
| Functions     | 20     | 20         |
