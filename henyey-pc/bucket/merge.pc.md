## Pseudocode: crates/bucket/src/merge.rs

"Merging is the fundamental operation that maintains bucket list integrity."
"When buckets are merged, entries from a newer bucket 'shadow' entries from"
"an older bucket with the same key."

"CAP-0020 INITENTRY Semantics:"
" - INIT + DEAD = Both entries are annihilated (nothing output)"
" - DEAD + INIT = Becomes LIVE (recreation cancels tombstone)"
" - INIT + LIVE = Becomes INIT with new value (preserves init status)"

"When entries cross level boundaries during a spill, INIT entries are"
"'normalized' to LIVE. This is because the init status is only relevant"
"within the merge window where the entry was created."

---

### merge_buckets

"Merge two buckets into a new bucket."
"Note: This wrapper always normalizes INIT→LIVE for backward compatibility."

```
function merge_buckets(old_bucket, new_bucket,
                       keep_dead_entries, max_protocol_version):
  → merge_buckets(old_bucket, new_bucket,
      keep_dead_entries, max_protocol_version,
      normalize_init_entries=true)
```

---

### merge_buckets

"Merge two buckets with explicit normalization control."
"Set normalize_init_entries=true when merging spills (crossing level boundaries),"
"false for same-level merges (e.g., at level 0)."

```
function merge_buckets(old_bucket, new_bucket,
    keep_dead_entries, max_protocol_version,
    normalize_init_entries):
  → merge_with_shadows_impl(old_bucket, new_bucket,
      keep_dead_entries, max_protocol_version,
      normalize_init_entries,
      shadow_buckets=[], keep_shadowed_lifecycle=false,
      counters=null)
```

---

### merge_with_shadows_impl

"Core merge implementation with integrated shadow checking."
"Performs a single-pass two-pointer merge with inline shadow filtering."
"When shadow_buckets is empty, shadow checking is a no-op."

```
function merge_with_shadows_impl(old_bucket, new_bucket,
    keep_dead_entries, max_protocol_version,
    normalize_init_entries, shadow_buckets,
    keep_shadowed_lifecycle_entries, counters):

  NOTE: "We intentionally do NOT use fast paths for empty buckets."
  NOTE: "stellar-core always goes through the full merge process"
  NOTE: "even when one input is empty, because output bucket gets"
  NOTE: "new metadata (protocol version) and the bucket hash"
  NOTE: "includes metadata."
  GUARD both buckets empty  → empty bucket

  old_iter = old_bucket.iter()
  new_iter = new_bucket.iter()

  old_meta = null
  new_meta = null
  old_current = advance_skip_metadata(old_iter, old_meta)
  new_current = advance_skip_metadata(new_iter, new_meta)

  (_, output_meta) = build_output_metadata(
      old_meta, new_meta, max_protocol_version)

  merged = []

  "For protocol >= 12 (FIRST_PROTOCOL_SHADOWS_REMOVED),"
  "shadow_buckets is always empty, so no cursors are created."
  shadow_cursors = [ShadowCursor(b) for b in shadow_buckets]

  if output_meta is not null:
    merged.append(output_meta)

  // --- Phase 1: Two-pointer merge ---
  while old_current exists AND new_current exists:
    old_key = old_current.key()
    new_key = new_current.key()

    if old_key < new_key:
      if should_keep_entry(old_current, keep_dead_entries):
        record_entry_type(counters, old_current)
        maybe_put(old_current, shadow_cursors,
            keep_shadowed_lifecycle_entries, merged, counters)
      old_current = next_non_meta(old_iter)

    else if old_key > new_key:
      if should_keep_entry(new_current, keep_dead_entries):
        entry = maybe_normalize_entry(new_current,
            normalize_init_entries)
        record_entry_type(counters, entry)
        maybe_put(entry, shadow_cursors,
            keep_shadowed_lifecycle_entries, merged, counters)
      new_current = next_non_meta(new_iter)

    else:  // keys equal — new shadows old
      merged_entry = merge_entries(old_current, new_current,
          keep_dead_entries, normalize_init_entries)
      if merged_entry is not null:
        record_entry_type(counters, merged_entry)
        maybe_put(merged_entry, shadow_cursors,
            keep_shadowed_lifecycle_entries, merged, counters)
      else:
        // INIT+DEAD annihilation
        counters.record_annihilated()
      old_current = next_non_meta(old_iter)
      new_current = next_non_meta(new_iter)

    // skip entries with no key (metadata)
    // (handles None key cases by advancing)

  // --- Phase 2: Drain remaining old entries ---
  while old_current exists:
    if not old_current.is_metadata()
        AND should_keep_entry(old_current, keep_dead_entries):
      record_entry_type(counters, old_current)
      maybe_put(old_current, shadow_cursors,
          keep_shadowed_lifecycle_entries, merged, counters)
    old_current = old_iter.next()

  // --- Phase 3: Drain remaining new entries ---
  while new_current exists:
    if not new_current.is_metadata()
        AND should_keep_entry(new_current, keep_dead_entries):
      entry = maybe_normalize_entry(new_current,
          normalize_init_entries)
      record_entry_type(counters, entry)
      maybe_put(entry, shadow_cursors,
          keep_shadowed_lifecycle_entries, merged, counters)
    new_current = new_iter.next()

  // --- Phase 4: Finalize ---
  if merged is empty:
    "In stellar-core, even a merge with no data entries still"
    "produces a bucket with a metadata entry (for protocol 11+)."
    "This ensures bucket list hash is consistent."
    if output_meta is not null:
      → Bucket.from_sorted_entries([output_meta])
    → empty bucket

  → Bucket.from_sorted_entries(merged)
```

**Calls:** [advance_skip_metadata](#helper-advance_skip_metadata), [build_output_metadata](#helper-build_output_metadata), [should_keep_entry](#helper-should_keep_entry), [maybe_normalize_entry](#helper-maybe_normalize_entry), [merge_entries](#merge_entries), [maybe_put](#helper-maybe_put), [next_non_meta](#helper-next_non_meta), [ShadowCursor](#helper-shadowcursor)

---

### merge_buckets_to_file

"Merge two buckets and write the output directly to an uncompressed XDR file."
"This is the fully streaming merge: both inputs and the output are streamed,"
"so memory usage is O(1) per input bucket regardless of size."

```
function merge_buckets_to_file(old_bucket, new_bucket,
    output_path, keep_dead_entries, max_protocol_version,
    normalize_init_entries):
  → merge_buckets_to_file(
      old_bucket, new_bucket, output_path,
      keep_dead_entries, max_protocol_version,
      normalize_init_entries, counters=null)
```

---

### merge_buckets_to_file

"Streaming merge to file with optional merge counters."

```
function merge_buckets_to_file(old_bucket,
    new_bucket, output_path, keep_dead_entries,
    max_protocol_version, normalize_init_entries, counters):

  GUARD both buckets empty  → create empty file, return (ZERO_HASH, 0)

  old_iter = old_bucket.iter()
  new_iter = new_bucket.iter()

  old_meta = null
  new_meta = null
  old_current = advance_skip_metadata(old_iter, old_meta)
  new_current = advance_skip_metadata(new_iter, new_meta)

  (_, output_meta) = build_output_metadata(
      old_meta, new_meta, max_protocol_version)

  writer = buffered_file_writer(output_path)
  hasher = SHA256()
  entry_count = 0

  // --- Helper: write one entry to file ---
  function write_entry(entry, writer, hasher, count):
    xdr_data = serialize_to_xdr(entry)
    CONST RECORD_MARK_BIT = 0x80000000
    record_mark = len(xdr_data) | RECORD_MARK_BIT
    writer.write(record_mark as big-endian u32)
    writer.write(xdr_data)
    hasher.update(record_mark as big-endian u32)
    hasher.update(xdr_data)
    count += 1

  // Write metadata first
  if output_meta is not null:
    record_entry_type(counters, output_meta)
    write_entry(output_meta, writer, hasher, entry_count)

  // --- Two-pointer merge (same as merge_with_shadows_impl) ---
  // but writing each entry to file instead of accumulating
  while old_current exists AND new_current exists:
    old_key = old_current.key()
    new_key = new_current.key()

    if old_key < new_key:
      if should_keep_entry(old_current, keep_dead_entries):
        record_entry_type(counters, old_current)
        write_entry(old_current, writer, hasher, entry_count)
      old_current = next_non_meta(old_iter)

    else if old_key > new_key:
      if should_keep_entry(new_current, keep_dead_entries):
        entry = maybe_normalize_entry(new_current,
            normalize_init_entries)
        record_entry_type(counters, entry)
        write_entry(entry, writer, hasher, entry_count)
      new_current = next_non_meta(new_iter)

    else:  // keys equal
      merged_entry = merge_entries(old_current, new_current,
          keep_dead_entries, normalize_init_entries)
      if merged_entry is not null:
        record_entry_type(counters, merged_entry)
        write_entry(merged_entry, writer, hasher, entry_count)
      else:
        counters.record_annihilated()
      old_current = next_non_meta(old_iter)
      new_current = next_non_meta(new_iter)

  // Drain remaining old entries
  while old_current exists:
    if not is_metadata AND should_keep_entry(old_current, keep_dead_entries):
      record_entry_type(counters, old_current)
      write_entry(old_current, writer, hasher, entry_count)
    old_current = old_iter.next()

  // Drain remaining new entries
  while new_current exists:
    if not is_metadata AND should_keep_entry(new_current, keep_dead_entries):
      entry = maybe_normalize_entry(new_current,
          normalize_init_entries)
      record_entry_type(counters, entry)
      write_entry(entry, writer, hasher, entry_count)
    new_current = new_iter.next()

  writer.flush()
  writer.sync_all()
  hash = hasher.finalize()
  → (hash, entry_count)
```

**Calls:** [advance_skip_metadata](#helper-advance_skip_metadata), [build_output_metadata](#helper-build_output_metadata), [should_keep_entry](#helper-should_keep_entry), [maybe_normalize_entry](#helper-maybe_normalize_entry), [merge_entries](#merge_entries)

---

### merge_in_memory

"In-memory merge of two buckets, avoiding disk I/O."
"Rust equivalent of stellar-core LiveBucket::mergeInMemory."
"Both input buckets MUST have in-memory entries available."

```
function merge_in_memory(old_bucket, new_bucket,
    max_protocol_version):

  ASSERT: old_bucket.has_in_memory_entries()
  ASSERT: new_bucket.has_in_memory_entries()

  old_entries = old_bucket.get_in_memory_entries()
  new_entries = new_bucket.get_in_memory_entries()

  // Build output metadata directly from max_protocol_version.
  // "This matches stellar-core mergeInMemory behavior where"
  // "meta.ledgerVersion = maxProtocolVersion without calling"
  // "calculateMergeProtocolVersion."
  @version(≥ FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY):
    output_meta = Metadata {
      ledger_version: max_protocol_version
      ext: V0
    }
    @version(≥ FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION):
      output_meta.ext = V1(BucketListType.Live)
  @version(< FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY):
    output_meta = null

  hasher = SHA256()
  key_index = {}  // maps serialized_key → entry_index

  capacity = len(old_entries) + len(new_entries)
             + (1 if output_meta else 0)
  all_entries = [] with capacity
  entry_idx = 0
  entry_buf = reusable buffer  // avoids repeated allocations
  key_buf = reusable buffer

  // --- Helper: add entry with incremental hashing ---
  function add_entry(entry, hasher, key_index,
      all_entries, entry_idx, entry_buf, key_buf):
    xdr_data = serialize_to_xdr(entry, into=entry_buf)
    CONST RECORD_MARK_BIT = 0x80000000
    record_mark = len(xdr_data) | RECORD_MARK_BIT
    hasher.update(record_mark as big-endian u32)
    hasher.update(xdr_data)
    if not entry.is_metadata():
      if entry has key:
        serialized_key = serialize_to_xdr(key, into=key_buf)
        key_index[serialized_key] = entry_idx
    all_entries.append(entry)
    entry_idx += 1

  // Add metadata first
  if output_meta is not null:
    add_entry(output_meta, ...)

  // Skip metadata entries in inputs
  old_idx = skip past metadata entries in old_entries
  new_idx = skip past metadata entries in new_entries

  // Level 0 always keeps tombstones, never normalizes INIT
  keep_dead_entries = true
  normalize_init_entries = false

  // --- Two-pointer merge ---
  while old_idx < len(old_entries)
      AND new_idx < len(new_entries):
    old_entry = old_entries[old_idx]
    new_entry = new_entries[new_idx]
    old_key = old_entry.key()
    new_key = new_entry.key()

    if old_key < new_key:
      if should_keep_entry(old_entry, keep_dead_entries):
        add_entry(old_entry, ...)
      old_idx += 1

    else if old_key > new_key:
      if should_keep_entry(new_entry, keep_dead_entries):
        add_entry(maybe_normalize_entry(new_entry,
            normalize_init_entries), ...)
      new_idx += 1

    else:  // keys equal
      merged = merge_entries(old_entry, new_entry,
          keep_dead_entries, normalize_init_entries)
      if merged is not null:
        add_entry(merged, ...)
      old_idx += 1
      new_idx += 1

  // Drain remaining old entries
  while old_idx < len(old_entries):
    entry = old_entries[old_idx]
    if not is_metadata AND should_keep_entry(entry, keep_dead_entries):
      add_entry(entry, ...)
    old_idx += 1

  // Drain remaining new entries
  while new_idx < len(new_entries):
    entry = new_entries[new_idx]
    if not is_metadata AND should_keep_entry(entry, keep_dead_entries):
      add_entry(maybe_normalize_entry(entry,
          normalize_init_entries), ...)
    new_idx += 1

  // Finalize
  if all_entries is empty:
    if output_meta is not null:
      → Bucket.from_sorted_entries_with_in_memory([output_meta])
    → empty bucket

  hash = hasher.finalize()
  metadata_count = 1 if output_meta else 0

  // "Use shared level zero state — no cloning needed!"
  → Bucket.from_parts(hash, all_entries, key_index,
      metadata_count)
```

**Calls:** [should_keep_entry](#helper-should_keep_entry), [maybe_normalize_entry](#helper-maybe_normalize_entry), [merge_entries](#merge_entries)

---

### merge_buckets

"Merge two buckets with shadow elimination for pre-shadow-removal protocols."
"Shadows are only used before protocol 12."

```
function merge_buckets(old_bucket,
    new_bucket, keep_dead_entries, max_protocol_version,
    normalize_init_entries, shadow_buckets):
  → merge_buckets(
      ..., counters=null)
```

---

### merge_buckets

```
function merge_buckets(
    old_bucket, new_bucket, keep_dead_entries,
    max_protocol_version, normalize_init_entries,
    shadow_buckets, counters):

  "For protocol >= 12 (FIRST_PROTOCOL_SHADOWS_REMOVED),"
  "shadows are always empty in practice."
  if shadow_buckets is empty
      OR max_protocol_version >= FIRST_PROTOCOL_SHADOWS_REMOVED:
    → merge_with_shadows_impl(..., shadow_buckets=[],
        keep_shadowed_lifecycle=false, counters)

  keep_shadowed_lifecycle =
    max_protocol_version >=
      FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY
  → merge_with_shadows_impl(..., shadow_buckets,
      keep_shadowed_lifecycle, counters)
```

---

### merge_entries

"Merge two entries with the same key. Returns merged entry or null for annihilation."

```
function merge_entries(old, new, keep_dead_entries,
    normalize_init_entries):

  // "CAP-0020: INITENTRY + DEADENTRY → Both annihilated"
  // "This is a key optimization: if we created and then deleted"
  // "in the same merge window, we output nothing at all."
  if old is INIT and new is DEAD:
    → null  // annihilation

  // "CAP-0020: DEADENTRY + INITENTRY=x → Output as LIVEENTRY=x"
  // "The old tombstone is cancelled by the new creation"
  if old is DEAD and new is INIT(entry):
    → LIVE(entry)

  // "CAP-0020: INITENTRY=x + LIVEENTRY=y → Output as INITENTRY=y"
  // "Preserve the INIT status (entry was created in this merge range)"
  if old is INIT and new is LIVE(entry):
    → INIT(entry)

  // New Live shadows old Live — new wins
  if old is LIVE and new is LIVE(entry):
    → LIVE(entry)

  // New Live shadows old Dead — live wins
  if old is DEAD and new is LIVE(entry):
    → LIVE(entry)

  // Any old + new INIT (not covered above)
  if new is INIT(entry):
    if normalize_init_entries:
      → LIVE(entry)
    else:
      → INIT(entry)

  // LIVEENTRY + DEADENTRY → tombstone or nothing
  if old is LIVE and new is DEAD(key):
    if keep_dead_entries:
      → DEAD(key)
    else:
      → null

  // Dead shadows Dead — keep newest if needed
  if old is DEAD and new is DEAD(key):
    if keep_dead_entries:
      → DEAD(key)
    else:
      → null

  // Metadata shouldn't have matching keys
  if old is METADATA or new is METADATA:
    → null
```

---

### merge_multiple

"Merge multiple buckets in order (first is oldest)."

```
function merge_multiple(buckets, keep_dead_entries,
    max_protocol_version):
  GUARD buckets is empty  → empty bucket

  result = buckets[0]
  for each bucket in buckets[1..]:
    result = merge_buckets(result, bucket,
        keep_dead_entries, max_protocol_version)
  → result
```

**Calls:** [merge_buckets](#merge_buckets)

---

### MergeIterator

"Iterator that yields merged entries from two buckets (lazy/streaming)."
"Always normalizes INIT entries to LIVE."

```
struct MergeIterator:
  old_entries: list of BucketEntry
  new_entries: list of BucketEntry
  old_idx: int
  new_idx: int
  keep_dead_entries: bool
  output_metadata: BucketEntry or null
```

### MergeIterator.new

```
function MergeIterator.new(old_bucket, new_bucket,
    keep_dead_entries, max_protocol_version):
  old_entries = collect all entries from old_bucket
  new_entries = collect all entries from new_bucket
  old_meta = extract_metadata(old_entries)
  new_meta = extract_metadata(new_entries)
  (_, output_metadata) = build_output_metadata(
      old_meta, new_meta, max_protocol_version)
  → MergeIterator { old_entries, new_entries,
      old_idx=0, new_idx=0, keep_dead_entries,
      output_metadata }
```

### MergeIterator.next

```
function MergeIterator.next():
  // Emit metadata first (once)
  if output_metadata is not null:
    meta = output_metadata
    output_metadata = null
    skip metadata entries in old_entries, new_entries
    → meta

  loop:
    GUARD both iterators exhausted  → null

    // Only old entries left
    if new_idx exhausted:
      entry = old_entries[old_idx]; old_idx += 1
      if not is_metadata:
        → entry
      continue

    // Only new entries left
    if old_idx exhausted:
      entry = new_entries[new_idx]; new_idx += 1
      if not is_metadata
          AND should_keep_entry(entry, keep_dead_entries):
        → normalize_entry(entry)
      continue

    // Both have entries — compare keys
    old_entry = old_entries[old_idx]
    new_entry = new_entries[new_idx]
    old_key = old_entry.key()
    new_key = new_entry.key()

    if old_key < new_key:
      old_idx += 1
      → old_entry

    else if old_key > new_key:
      new_idx += 1
      if should_keep_entry(new_entry, keep_dead_entries):
        → normalize_entry(new_entry)
      continue

    else:  // keys equal
      old_idx += 1
      new_idx += 1
      merged = merge_entries(old_entry, new_entry,
          keep_dead_entries, normalize=true)
      if merged is not null:
        → merged
      continue
```

---

### Helper: advance_skip_metadata

```
function advance_skip_metadata(iter, meta_out):
  for each entry in iter:
    if entry is Metadata:
      meta_out = entry.metadata
      continue
    → entry
  → null
```

---

### Helper: build_output_metadata

"Calculate merge protocol version as max of input bucket versions."
"Matches stellar-core's calculateMergeProtocolVersion() in BucketBase.cpp."

```
function build_output_metadata(old_meta, new_meta,
    max_protocol_version):

  protocol_version = 0
  if old_meta exists:
    protocol_version = max(protocol_version,
        old_meta.ledger_version)
  if new_meta exists:
    protocol_version = max(protocol_version,
        new_meta.ledger_version)

  GUARD protocol_version > max_protocol_version (and max > 0)
    → error "bucket protocol version exceeds max"

  if protocol_version
      < FIRST_PROTOCOL_SUPPORTING_INITENTRY_AND_METAENTRY:
    → (protocol_version, null)

  output = Metadata {
    ledger_version: protocol_version,
    ext: V0
  }

  "For Protocol 23+, Live buckets must use V1 extension"
  "with BucketListType::LIVE."
  @version(≥ FIRST_PROTOCOL_SUPPORTING_PERSISTENT_EVICTION):
    output.ext = V1(BucketListType.Live)

  → (protocol_version, Metadata(output))
```

---

### Helper: ShadowCursor

```
struct ShadowCursor:
  iter: BucketIter
  current: BucketEntry or null
```

```
function ShadowCursor.new(bucket):
  iter = bucket.iter()
  current = next_non_meta(iter)
  → ShadowCursor { iter, current }
```

```
function ShadowCursor.advance_to_key_or_after(key):
  loop:
    GUARD current is null  → false
    entry_key = current.key()
    if entry_key is null:
      current = next_non_meta(iter)
      continue
    if entry_key < key:
      current = next_non_meta(iter)
    else if entry_key == key:
      → true
    else:  // entry_key > key
      → false
```

---

### Helper: next_non_meta

```
function next_non_meta(iter):
  → first entry from iter that is not metadata
```

---

### Helper: is_shadowed

```
function is_shadowed(entry, cursors):
  GUARD entry has no key  → false
  key = entry.key()
  for each cursor in cursors:
    if cursor.advance_to_key_or_after(key):
      → true
  → false
```

---

### Helper: maybe_put

"Matches stellar-core's BucketOutputIterator::maybePut() pattern."
"If the entry is shadowed by a higher-level bucket, it's silently dropped"
"unless it's a lifecycle entry (INIT/DEAD) and keep_shadowed_lifecycle is true."

```
function maybe_put(entry, shadow_cursors,
    keep_shadowed_lifecycle, output, counters):
  if shadow_cursors is not empty:
    if keep_shadowed_lifecycle
        AND (entry is INIT or entry is DEAD):
      // "Lifecycle entries preserved even when shadowed"
      pass  // do not drop
    else if is_shadowed(entry, shadow_cursors):
      counters.record_shadowed()
      return  // drop entry
  output.append(entry)
```

**Calls:** [is_shadowed](#helper-is_shadowed)

---

### Helper: should_keep_entry

```
function should_keep_entry(entry, keep_dead_entries):
  if entry is DEAD:
    → keep_dead_entries
  → true
```

---

### Helper: normalize_entry

```
function normalize_entry(entry):
  if entry is INIT(data):
    → LIVE(data)
  → entry
```

---

### Helper: maybe_normalize_entry

```
function maybe_normalize_entry(entry, normalize):
  if normalize:
    → normalize_entry(entry)
  → entry
```

---

### Helper: record_entry_type

```
function record_entry_type(counters, entry):
  if counters is null: return
  if entry is Metadata: counters.record_new(Meta)
  if entry is Init:     counters.record_new(Init)
  if entry is Live:     counters.record_new(Live)
  if entry is Dead:     counters.record_new(Dead)
```

---

### Helper: extract_metadata

```
function extract_metadata(entries):
  → first Metadata entry found in entries, or null
```

---

## Summary

| Metric        | Source | Pseudocode |
|---------------|--------|------------|
| Lines (logic) | ~1327  | ~370       |
| Functions     | 20     | 20         |
