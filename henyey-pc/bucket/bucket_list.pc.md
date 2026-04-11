## Pseudocode: crates/bucket/src/bucket_list.rs

"The BucketList is Stellar's core data structure for storing ledger state."
"It consists of 11 levels (0-10), where each level contains two buckets:"
"  curr: The current bucket being filled with new entries"
"  snap: The snapshot bucket from the previous spill"

"Level 0:  [curr] [snap]   <- Updates every 2 ledgers"
"Level 1:  [curr] [snap]   <- Updates every 8 ledgers"
"Level 2:  [curr] [snap]   <- Updates every 32 ledgers"
"..."
"Level 10: [curr] [snap]   <- Never spills (top level)"

"Lookups search from level 0 to level 10, checking curr then snap."
"The first match is returned (newer entries shadow older)."
"Dead entries (tombstones) shadow live entries, returning None."

CONST BUCKET_LIST_LEVELS = 11

CONST HAS_NEXT_STATE_CLEAR = 0   // No pending merge
CONST HAS_NEXT_STATE_OUTPUT = 1  // Merge complete, output hash known
CONST HAS_NEXT_STATE_INPUTS = 2  // Merge in progress, input hashes stored

---

### HasNextState

"State of a pending bucket merge from History Archive State (HAS)."

```
struct HasNextState:
  state: u32        // 0=clear, 1=output, 2=inputs
  output: Hash256 or null
  input_curr: Hash256 or null
  input_snap: Hash256 or null
```

---

STATE_MACHINE: PendingMerge
  STATES: [InMemory, Async]
  TRANSITIONS:
    InMemory → (terminal): commit() promotes to curr
    Async → (terminal): resolve() blocks until done, then commit() promotes

### PendingMerge

```
enum PendingMerge:
  InMemory(Bucket)      // synchronous result (level 0)
  Async(AsyncMergeHandle) // background merge (levels 1+)
```

```
function PendingMerge.hash():
  if InMemory(bucket):
    → bucket.hash()
  if Async(handle):
    if handle has cached result:
      → result.hash()
    → ZERO_HASH  // unresolved
```

### PendingMergeState

"Describes the serializable state of a pending merge for HAS persistence."

```
enum PendingMergeState:
  Output(Hash256)                  // state 1: merge done
  Inputs { curr: Hash256, snap: Hash256 }  // state 2: merge in progress
```

---

### AsyncMergeHandle

"Handle to an asynchronous bucket merge running in a background thread."

```
struct AsyncMergeHandle:
  receiver: channel     // receives merge result
  level: int
  result: Bucket or null  // cached after resolution
  input_file_paths: list of paths  // prevents GC while merge runs
  input_curr_hash: Hash256
  input_snap_hash: Hash256
  merge_key: MergeKey
```

### AsyncMergeHandle.start_merge

```
function AsyncMergeHandle.start_merge(curr, snap,
    keep_dead_entries, protocol_version, normalize_init,
    shadow_buckets, level, bucket_dir, counters):

  (sender, receiver) = create_channel()

  "Capture input hashes BEFORE the merge starts. These are"
  "needed for HAS serialization: if the merge is still in"
  "progress when we persist the HAS, we store these as"
  "state=2 (FB_HASH_INPUTS)."
  input_curr_hash = curr.hash()
  input_snap_hash = snap.hash()

  // Capture input bucket file paths for GC tracking
  input_file_paths = collect backing paths from
      [curr, snap, shadow_buckets...]

  spawn_blocking:
    if bucket_dir is not null:
      // Disk-backed merge
      temp_path = temp_merge_path(bucket_dir)
      (hash, entry_count) = merge_buckets_to_file(
          curr, snap, temp_path, keep_dead_entries,
          protocol_version, normalize_init, counters)
      REF: merge::merge_buckets_to_file

      if entry_count == 0:
        remove temp_path
        result = empty bucket
      else:
        permanent_path = bucket_dir / canonical_filename(hash)
        if not permanent_path.exists():
          rename temp_path → permanent_path
          result = Bucket.from_xdr_file_disk_backed(permanent_path)
        else:
          remove temp_path
          result = Bucket.from_xdr_file_disk_backed(permanent_path)
    else:
      // In-memory merge
      result = merge_with_options_and_shadows_and_counters(
          curr, snap, keep_dead_entries, protocol_version,
          normalize_init, shadow_buckets, counters)
      REF: merge::merge_buckets

    counters.record_merge_completed(elapsed)
    sender.send(result)

  merge_key = MergeKey(keep_dead_entries,
      input_curr_hash, input_snap_hash)

  → AsyncMergeHandle { receiver, level, result=null,
      input_file_paths, input_curr_hash, input_snap_hash,
      merge_key }
```

### AsyncMergeHandle.resolve

```
function AsyncMergeHandle.resolve():
  if result is not null:
    → result

  GUARD receiver already consumed  → error

  // block_in_place: allows blocking from async context
  bucket = receiver.blocking_recv()
  result = bucket
  → result
```

---

### BucketLevel

"A single level in the BucketList, containing curr and snap buckets."

```
struct BucketLevel:
  curr: Bucket     // current bucket
  snap: Bucket     // snapshot bucket
  next: PendingMerge or null  // pending merge result
  level: int       // 0-10
```

### BucketLevel.new

```
function BucketLevel.new(level):
  → BucketLevel {
    curr: empty_bucket,
    snap: empty_bucket,
    next: null,
    level: level
  }
```

### BucketLevel.hash

"SHA256(curr_hash || snap_hash)"
"Matches stellar-core's BucketLevel::getHash()."

```
function BucketLevel.hash():
  → SHA256(curr.hash() || snap.hash())
```

### BucketLevel.commit

"Promote the prepared bucket into curr, if any."
"For async merges, this will block until the merge completes."

```
function BucketLevel.commit():
  if next is null:
    → null

  pending = next
  next = null

  if pending is InMemory(bucket):
    curr = bucket
    → null

  if pending is Async(handle):
    merge_key = handle.merge_key
    bucket = handle.resolve()
    if success:
      output_hash = bucket.hash()
      curr = bucket
      → (merge_key, output_hash)
    else:
      // Keep current bucket on error
      → null
```

### BucketLevel.pending_merge_state

"Get the full pending merge state for HAS serialization."

```
function BucketLevel.pending_merge_state():
  if next is null:
    → null
  if next is InMemory(bucket):
    h = bucket.hash()
    if h is zero: → null
    → PendingMergeState.Output(h)
  if next is Async(handle):
    if handle.result exists:
      h = handle.result.hash()
      if h is zero: → null
      → PendingMergeState.Output(h)
    else:
      // Still in progress → state 2 (input hashes)
      → PendingMergeState.Inputs {
        curr: handle.input_curr_hash,
        snap: handle.input_snap_hash
      }
```

### BucketLevel.snap

"Implements spill behavior: curr→snap, clear curr, return new snap."

```
function BucketLevel.snap():
  old_curr = curr
  curr = empty_bucket
  snap = old_curr
  → snap  // flows to next level
```

### BucketLevel.prepare_with_normalization

"Prepare the next bucket for this level."
"Merges curr (or empty) with incoming bucket."

```
function BucketLevel.prepare_with_normalization(ledger_seq,
    protocol_version, incoming, keep_dead_entries,
    shadow_buckets, normalize_init, use_empty_curr,
    bucket_dir, merge_map, merge_counters):

  GUARD next is not null  → error "merge already in progress"

  // "use_empty_curr: used when the level is about to snap"
  // "its curr (shouldMergeWithEmptyCurr)."
  curr_for_merge = if use_empty_curr:
    empty_bucket
  else:
    self.curr

  // Check merge map for cached result
  if merge_map is not null:
    key = MergeKey(keep_dead_entries,
        curr_for_merge.hash(), incoming.hash())
    cached_hash = merge_map.get_output(key)
    if cached_hash exists AND not zero:
      path = bucket_dir / canonical_filename(cached_hash)
      if path.exists():
        bucket = Bucket.from_xdr_file_disk_backed(path)
        if success:
          next = InMemory(bucket)
          return

  // For levels 1+: async merge (background thread)
  if level >= 1:
    handle = AsyncMergeHandle.start_merge(
        curr_for_merge, incoming, keep_dead_entries,
        protocol_version, normalize_init, shadow_buckets,
        level, bucket_dir, merge_counters)
    next = Async(handle)
  else:
    // Level 0: synchronous merge
    merged = merge_with_options_and_shadows_and_counters(
        curr_for_merge, incoming, keep_dead_entries,
        protocol_version, normalize_init, shadow_buckets,
        merge_counters)
    next = InMemory(merged)
```

**Calls:** [AsyncMergeHandle.start_merge](#asyncmergehandlestart_merge), merge_buckets (REF: merge.rs)

### BucketLevel.prepare_first_level

"Level 0 in-memory merge optimization."
"Avoids disk I/O and keeps entries in memory for subsequent fast merges."

```
function BucketLevel.prepare_first_level(protocol_version,
    incoming):

  GUARD level != 0  → error "only level 0"
  GUARD next is not null  → error "merge already in progress"

  can_use_in_memory =
    curr.has_in_memory_entries()
    AND incoming.has_in_memory_entries()

  if can_use_in_memory:
    merged = merge_in_memory(curr, incoming,
        protocol_version)
    REF: merge::merge_in_memory
  else:
    // Fallback to regular merge
    // "Level 0 always keeps tombstones and never"
    // "normalizes INIT entries"
    merged = merge_with_options_and_shadows(
        curr, incoming,
        keep_dead=true, protocol_version,
        normalize_init=false, shadow_buckets=[])

  // Ensure result has in-memory entries for next merge
  if not merged.has_in_memory_entries():
    entries = collect all from merged.iter()
    merged = Bucket.from_sorted_entries_with_in_memory(entries)

  next = InMemory(merged)
```

---

### BucketList

"The complete BucketList structure representing all ledger state."

```
struct BucketList:
  levels: list of BucketLevel  // 11 levels
  ledger_seq: u32
  bucket_dir: path or null     // for disk-backed merge output
  bucket_list_db_config: config or null
  completed_merges: list of (MergeKey, Hash256)
  merge_map: BucketMergeMap or null  // deduplication cache
  merge_counters: MergeCounters
```

### BucketList.new

```
function BucketList.new():
  levels = [BucketLevel.new(i) for i in 0..11]
  → BucketList { levels, ledger_seq=0, ... }
```

### BucketList.hash

"Compute the Merkle root hash: SHA256 of all level hashes."

```
function BucketList.hash():
  hasher = SHA256()
  for each level in levels:
    hasher.update(level.hash())
  → hasher.finalize()
```

### BucketList.get

"Look up an entry by key. Searches newest to oldest."

```
function BucketList.get(key):
  → get_with_debug(key, debug=false)
```

### BucketList.get_with_debug

"Searches from level 0 to level 10, curr then snap."
"Pending merges (next) are NOT part of bucket list state yet."

```
function BucketList.get_with_debug(key, debug):
  for each (level_idx, level) in levels:
    // Check curr first
    entry = level.curr.get(key)
    if entry found:
      if entry is LIVE or INIT:
        → entry.ledger_entry
      if entry is DEAD:
        → null  // entry is deleted
      if entry is METADATA:
        continue

    // Then check snap
    entry = level.snap.get(key)
    if entry found:
      if entry is LIVE or INIT:
        → entry.ledger_entry
      if entry is DEAD:
        → null
      if entry is METADATA:
        continue

  → null  // not found in any bucket
```

### BucketList.add_batch

"Add ledger entries from a newly closed ledger."
"Mirrors stellar-core's bucket list update pipeline."

```
function BucketList.add_batch(ledger_seq, protocol_version,
    bucket_list_type, init_entries, live_entries, dead_entries):

  use_init = protocol_version
      >= FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY

  entries = []

  // Build metadata entry
  @version(≥ FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY):
    meta = Metadata {
      ledger_version: protocol_version,
      ext: V0
    }
    @version(≥ FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION):
      meta.ext = V1(bucket_list_type)
    entries.append(Metadata(meta))

  // Deduplicate and add init entries
  dedup_init = deduplicate_entries(init_entries)
  if use_init:
    entries.extend(INIT(e) for e in dedup_init)
  else:
    entries.extend(LIVE(e) for e in dedup_init)

  // Deduplicate and add live entries
  dedup_live = deduplicate_entries(live_entries)
  entries.extend(LIVE(e) for e in dedup_live)

  // Deduplicate and add dead entries
  dedup_dead = unique dead_entries by serialized key
  entries.extend(DEAD(k) for k in dedup_dead)

  // "fresh_in_memory_only() skips hash computation because"
  // "this bucket will be immediately merged with level 0 curr."
  // "Only the merged result's hash matters."
  entries.sort_by(compare_entries)
  new_bucket = Bucket.fresh_in_memory_only(entries)

  add_batch_internal(ledger_seq, protocol_version, new_bucket)
  self.ledger_seq = ledger_seq
  maybe_initialize_caches()
```

**Calls:** [deduplicate_entries](#helper-deduplicate_entries), [add_batch_internal](#bucketlistadd_batch_internal)

### BucketList.add_batch_internal

"Core bucket list update pipeline."
"Matches stellar-core's BucketListBase::addBatchInternal."

```
function BucketList.add_batch_internal(ledger_seq,
    protocol_version, new_bucket):

  GUARD ledger_seq == 0  → error "must be > 0"

  completed_merges.clear()

  // --- Step 1: Process spills highest→lowest (10→1) ---
  // "By processing from highest to lowest, we ensure each"
  // "level's curr is available to be snapped before any"
  // "modifications occur."
  for i in (BUCKET_LIST_LEVELS-1) down to 1:
    if level_should_spill(ledger_seq, i - 1):
      // Snap level i-1: moves curr→snap, returns new snap
      spilling_snap = levels[i - 1].snap()

      // Clear in-memory entries when leaving level 0
      // "Prevents memory leak where Arc<Vec<BucketEntry>>"
      // "references would accumulate across generations."
      if i - 1 == 0:
        spilling_snap.clear_in_memory_entries()
        levels[0].snap.clear_in_memory_entries()

      // Commit any pending merge at level i (next→curr)
      if (merge_key, output_hash) = levels[i].commit():
        completed_merges.append((merge_key, output_hash))

      // Prepare level i: merge curr with spilling_snap
      keep_dead = keep_tombstone_entries(i)
      normalize_init = false
      use_empty_curr = should_merge_with_empty_curr(
          ledger_seq, i)

      @version(< FIRST_PROTOCOL_SHADOWS_REMOVED):
        shadow_buckets = collect curr+snap from levels 0..i-1
      @version(≥ FIRST_PROTOCOL_SHADOWS_REMOVED):
        shadow_buckets = []

      levels[i].prepare_with_normalization(
          ledger_seq, protocol_version, spilling_snap,
          keep_dead, shadow_buckets, normalize_init,
          use_empty_curr, bucket_dir, merge_map,
          merge_counters)

  // --- Step 2: Apply new entries to level 0 ---
  levels[0].prepare_first_level(protocol_version, new_bucket)
  levels[0].commit()

  // --- Step 3: Persist in-memory buckets to disk ---
  // "Ensure all curr/snap buckets have a permanent file"
  // "on disk so that restart recovery can locate them by hash."
  if bucket_dir is not null:
    for each level in levels:
      for each bucket in [level.curr, level.snap]:
        if bucket has no backing file AND hash is not zero:
          permanent = bucket_dir / canonical_filename(hash)
          if not permanent.exists():
            bucket.save_to_xdr_file(permanent)
```

**Calls:** [level_should_spill](#bucketlistlevel_should_spill), [should_merge_with_empty_curr](#bucketlistshould_merge_with_empty_curr), [keep_tombstone_entries](#helper-keep_tombstone_entries), [BucketLevel.snap](#bucketlevelsnap), [BucketLevel.commit](#bucketlevelcommit), [BucketLevel.prepare_with_normalization](#bucketlevelprepare_with_normalization), [BucketLevel.prepare_first_level](#bucketlevelprepare_first_level)

---

### BucketList.advance_to_ledger

"Advance bucket list by applying empty batches for intermediate ledgers."
"Required because spill boundaries depend on being called for every ledger."

```
function BucketList.advance_to_ledger(target_ledger,
    protocol_version, bucket_list_type):
  GUARD target_ledger <= current  → ok (nothing to do)

  for seq in (current + 1) to (target_ledger - 1):
    add_batch(seq, protocol_version, bucket_list_type,
        [], [], [])
```

---

### BucketList.level_should_spill

"Returns true if a level should spill at a given ledger."
"Matches stellar-core's levelShouldSpill."

```
function BucketList.level_should_spill(ledger_seq, level):
  // Top level never spills
  if level == BUCKET_LIST_LEVELS - 1:
    → false

  half = level_half(level)
  size = level_size(level)
  → (ledger_seq % half == 0) OR (ledger_seq % size == 0)
```

### Helper: level_half

"Half the idealized size of a level."
"Level 0: 2, Level 1: 8, Level 2: 32, ..."

```
function level_half(level):
  → 1 << (2 * level + 1)
```

### Helper: level_size

"Idealized size of a level for spill boundaries."
"Level 0: 4, Level 1: 16, Level 2: 64, ..."

```
function level_size(level):
  → 1 << (2 * (level + 1))
```

### Helper: round_down

```
function round_down(value, modulus):
  if modulus == 0: → 0
  → value AND NOT(modulus - 1)
```

---

### BucketList.should_merge_with_empty_curr

"Prevents data duplication during spills."
"When curr is about to become snap, merge with empty instead."
"Matches stellar-core's shouldMergeWithEmptyCurr."

```
function BucketList.should_merge_with_empty_curr(
    ledger_seq, level):
  if level == 0:
    → false

  // When the merge was started
  merge_start = round_down(ledger_seq, level_half(level - 1))
  // When the next spill would happen
  next_change = merge_start + level_half(level - 1)

  // If next change causes this level to spill, use empty curr
  → level_should_spill(next_change, level)
```

### Helper: keep_tombstone_entries

```
function keep_tombstone_entries(level):
  → level < BUCKET_LIST_LEVELS - 1
```

---

### BucketList.scan_for_entries_of_types

"Scan bucket list for live entries matching given types."
"Single pass with per-key deduplication."

```
function BucketList.scan_for_entries_of_types(entry_types,
    callback):
  type_set = set(entry_types)
  seen_keys = {}  // HashSet<LedgerKey>

  for each level in levels:
    for each bucket in [level.curr, level.snap]:
      for each entry in bucket.iter():
        key = entry.key()
        if key is null or key in seen_keys:
          continue

        entry_type = type of entry's ledger data
        if entry_type not in type_set:
          continue

        seen_keys.insert(key)

        if not entry.is_dead():
          if not callback(entry):
            → false  // stopped early

  → true  // completed
```

---

### BucketList.restore_from_hashes

"Restore from flat array of bucket hashes (no pending merges)."

```
function BucketList.restore_from_hashes(hashes, load_bucket):
  GUARD len(hashes) != BUCKET_LIST_LEVELS * 2  → error

  pairs = chunk hashes into (curr, snap) pairs
  next_states = [default HasNextState] * BUCKET_LIST_LEVELS
  → restore_from_has(pairs, next_states, load_bucket)
```

### BucketList.restore_from_has

"Restore from HAS with full FutureBucket support."
"Handles state 1 (completed merge) by loading output bucket."

```
function BucketList.restore_from_has(hashes, next_states,
    load_bucket):
  GUARD len(hashes) != BUCKET_LIST_LEVELS  → error

  levels = []
  for each (i, (curr_hash, snap_hash)) in hashes:
    curr = if curr_hash is zero: empty else: load_bucket(curr_hash)
    snap = if snap_hash is zero: empty else: load_bucket(snap_hash)

    next = null
    if next_states[i].state == HAS_NEXT_STATE_OUTPUT:
      output_hash = next_states[i].output
      if output_hash exists and not zero:
        next = InMemory(load_bucket(output_hash))
    // state 2 handled later by restart_merges_from_has

    level = BucketLevel { curr, snap, next, level=i }
    levels.append(level)

  → BucketList { levels, ledger_seq=0, ... }
```

### BucketList.restart_merges_from_has

"Restart pending merges after HAS restore (parallel)."
"Handles state 2 by restarting merges with stored input hashes."

```
async function BucketList.restart_merges_from_has(ledger,
    protocol_version, next_states, load_bucket,
    restart_structure_based):

  // Phase 1: Collect work items (sequential)
  work_items = []
  for i in 1..BUCKET_LIST_LEVELS:
    if levels[i].next is not null: continue

    if next_states[i].state == HAS_NEXT_STATE_INPUTS:
      curr_hash = next_states[i].input_curr
      snap_hash = next_states[i].input_snap
      input_curr = load_bucket(curr_hash)
      input_snap = load_bucket(snap_hash)
      work_items.append({
        level: i,
        input_curr, input_snap,
        keep_dead: keep_tombstone_entries(i)
      })

  // Phase 2: Spawn all merges in parallel
  handles = []
  for each work in work_items:
    handle = spawn_blocking:
      perform_merge(work.input_curr, work.input_snap,
          bucket_dir, work.keep_dead, protocol_version)
    handles.append(handle)

  // Phase 3: Await all and install results
  for each (level, merged) in await_all(handles):
    levels[level].next = InMemory(merged)

  // Fall back to structure-based restart for state 0 levels
  if restart_structure_based:
    restart_merges(ledger, protocol_version)
  else:
    ledger_seq = ledger
```

**Calls:** [perform_merge](#helper-perform_merge), [restart_merges](#bucketlistrestart_merges)

### BucketList.restart_merges

"Structure-based merge restart: examines buckets to determine"
"which merges should be in progress."
"Matches stellar-core's BucketListBase::restartMerges()."

```
function BucketList.restart_merges(ledger, protocol_version):
  for i in 1..BUCKET_LIST_LEVELS:
    if levels[i].next is not null: continue

    prev_snap = levels[i - 1].snap
    if prev_snap is empty:
      break  // higher levels uninitialized

    merge_start = round_down(ledger, level_half(i - 1))
    merge_pv = prev_snap.protocol_version()
        or protocol_version
    keep_dead = keep_tombstone_entries(i)
    normalize_init = false
    use_empty_curr = should_merge_with_empty_curr(
        merge_start, i)

    levels[i].prepare_with_normalization(
        merge_start, merge_pv, prev_snap,
        keep_dead, shadow_buckets=[], normalize_init,
        use_empty_curr, bucket_dir, merge_map,
        merge_counters)

  ledger_seq = ledger
```

---

### BucketList.scan_for_eviction

"Scan for expired Soroban entries in the bucket list."
"Persistent entries → archive; Temporary entries → delete."

```
function BucketList.scan_for_eviction(current_ledger):
  archived = []
  deleted_keys = []
  seen_keys = {}

  for each level in levels:
    for each bucket in [level.curr, level.snap]:
      for each entry in bucket.iter():
        // Mark dead keys as seen
        if entry is DEAD:
          seen_keys.insert(entry.key)
          continue
        if entry is METADATA: continue

        live_entry = entry.ledger_entry
        if not is_soroban_entry(live_entry): continue

        key = ledger_entry_to_key(live_entry)
        if key in seen_keys: continue
        seen_keys.insert(key)

        ttl_key = get_ttl_key(key)
        ttl_entry = self.get(ttl_key)
        if ttl_entry is null: continue

        if not is_ttl_expired(ttl_entry, current_ledger):
          continue

        if is_temporary_entry(live_entry):
          deleted_keys.append(key)
        else if is_persistent_entry(live_entry):
          archived.append(live_entry)

  → (archived, deleted_keys)
```

### BucketList.scan_for_eviction_incremental

"Incremental eviction scan matching stellar-core's scanForEviction."
"Scans from iterator position, stops at eviction_scan_size bytes."

```
function BucketList.scan_for_eviction_incremental(iter,
    current_ledger, settings):

  result = EvictionResult {
    candidates: [], end_iterator: iter,
    bytes_scanned: 0, scan_complete: false
  }

  // Update iterator based on spills
  update_starting_eviction_iterator(iter,
      settings.starting_eviction_scan_level,
      current_ledger)

  start_iter = iter
  bytes_remaining = settings.eviction_scan_size
  seen_keys = {}

  loop:
    level = iter.bucket_list_level
    GUARD level >= BUCKET_LIST_LEVELS → done (wrapped)

    bucket = if iter.is_curr_bucket:
      levels[level].curr
    else:
      levels[level].snap

    (_, bytes_used, finished) = scan_bucket_region(
        bucket, iter, bytes_remaining, current_ledger,
        result.candidates, seen_keys)

    result.bytes_scanned += bytes_used
    bytes_remaining -= bytes_used

    if bytes_remaining <= 0:
      result.scan_complete = true
      break

    if finished:
      iter.advance_to_next_bucket(
          settings.starting_eviction_scan_level)
      // Check if completed full cycle
      if iter returned to start_iter position:
        result.scan_complete = true
        break

  result.end_iterator = iter
  → result
```

**Calls:** [scan_bucket_region](#bucketlistscan_bucket_region)

### BucketList.scan_bucket_region

"Scan a region of a bucket for evictable entries."
"Returns (entries_scanned, bytes_used, finished_bucket)."
"Uses byte-offset-aware iteration for disk-backed buckets."

```
function BucketList.scan_bucket_region(bucket, iter,
    max_bytes, current_ledger, candidates, seen_keys):

  entries_scanned = 0
  bytes_used = 0

  // Skip pre-Soroban buckets
  if bucket.protocol_version() < MIN_SOROBAN_PROTOCOL_VERSION:
    iter.bucket_file_offset = 0
    → (0, 0, true)

  start_offset = iter.bucket_file_offset

  for each (entry, entry_size) in
      bucket.iter_from_offset_with_sizes(start_offset):
    bytes_used += entry_size
    entries_scanned += 1

    // Process entry for eviction
    if entry is DEAD:
      seen_keys.insert(entry.key)
    else if entry is LIVE or INIT:
      live_entry = entry.ledger_entry
      if is_soroban_entry(live_entry):
        key = ledger_entry_to_key(live_entry)
        if key not in seen_keys:
          seen_keys.insert(key)
          ttl_key = get_ttl_key(key)
          ttl_entry = self.get(ttl_key)
          if ttl_entry exists
              AND is_ttl_expired(ttl_entry, current_ledger):
            is_temp = is_temporary_entry(live_entry)
            // "For persistent entries, archive the NEWEST"
            // "version from the bucket list."
            if not is_temp:
              entry_for_candidate = self.get(key)
                  or live_entry
            else:
              entry_for_candidate = live_entry
            candidates.append(EvictionCandidate {
              entry: entry_for_candidate,
              data_key: key, ttl_key,
              is_temporary: is_temp,
              position: iter with updated offset
            })

    if bytes_used >= max_bytes:
      iter.bucket_file_offset = start_offset + bytes_used
      → (entries_scanned, bytes_used, false)

  // Finished the bucket
  iter.bucket_file_offset = start_offset + bytes_used
  → (entries_scanned, bytes_used, true)
```

---

### BucketList.all_referenced_hashes

"All bucket hashes including pending merge inputs and outputs."
"Used for garbage collection."

```
function BucketList.all_referenced_hashes():
  hashes = []
  for each level in levels:
    hashes.append(level.curr.hash())
    hashes.append(level.snap.hash())
    if level.next is InMemory(bucket):
      hashes.append(bucket.hash())
    if level.next is Async(handle):
      hashes.append(handle.input_curr_hash)
      hashes.append(handle.input_snap_hash)
      if handle.result exists:
        hashes.append(handle.result.hash())
  → hashes
```

### BucketList.referenced_file_paths

"All file paths referenced by disk-backed buckets."
"Critical for GC — cannot delete files still being read by merges."

```
function BucketList.referenced_file_paths():
  paths = {}
  for each level in levels:
    if level.curr has backing file: paths.add(path)
    if level.snap has backing file: paths.add(path)
    if level.next is InMemory(bucket):
      if bucket has backing file: paths.add(path)
    if level.next is Async(handle):
      paths.add_all(handle.input_file_paths)
      if handle.result has backing file: paths.add(path)
  → paths
```

---

### BucketList.maybe_initialize_caches

"Initialize per-bucket caches for DiskIndex buckets."
"Each bucket gets proportional share of memory budget."

```
function BucketList.maybe_initialize_caches():
  if config is null or memory_for_caching_mb == 0:
    return

  counters = sum_bucket_entry_counters()
  total_account_bytes = counters.size_for_type(Account)
  for each level in levels:
    for each bucket in [level.curr, level.snap]:
      if not bucket.is_empty():
        bucket.maybe_initialize_cache(
            total_account_bytes, config)
```

---

### Helper: deduplicate_entries

"Deduplicate ledger entries by key, keeping last occurrence."

```
function deduplicate_entries(entries):
  // First pass: record position of each key
  key_positions = {}  // serialized_key → index
  for (idx, entry) in entries:
    key = ledger_entry_to_key(entry)
    key_bytes = serialize(key)
    key_positions[key_bytes] = idx  // last wins

  // Second pass: collect entries at recorded positions
  positions = set(key_positions.values())
  → [entry for (idx, entry) in entries
      if idx in positions]
```

### Helper: perform_merge

"Single bucket merge for restart_merges_from_has."

```
function perform_merge(input_curr, input_snap,
    bucket_dir, keep_dead, protocol_version):
  if bucket_dir is not null:
    temp_path = temp_merge_path(bucket_dir)
    (hash, count) = merge_buckets_to_file(
        input_curr, input_snap, temp_path,
        keep_dead, protocol_version, normalize_init=false)
    REF: merge::merge_buckets_to_file
    if count == 0:
      remove temp_path
      → empty bucket
    permanent = bucket_dir / canonical_filename(hash)
    rename temp_path → permanent (or use existing)
    → Bucket.from_xdr_file_disk_backed(permanent)
  else:
    → merge_with_options_and_shadows(
        input_curr, input_snap, keep_dead,
        protocol_version, normalize_init=false, shadows=[])
    REF: merge::merge_buckets
```

---

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | ~2830  | ~530       |
| Functions     | 38     | 38         |
