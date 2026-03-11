## Continuation Prompt for rs-soroban-env Typed API Completion

### Context

You are working on the `typed-invoke-host-function-api` branch of `https://github.com/tomerweller/rs-soroban-env`. The repo is already cloned at `/home/tomer/henyey-2/tmp/rs-soroban-env-typed-api/`. The branch is at commit `c0519e95` (HEAD).

This branch adds a `invoke_host_function_typed()` public API to `soroban-env-host/src/e2e_invoke.rs` — a typed alternative to the existing bytes-in/bytes-out `invoke_host_function()`. The purpose is to let pure-Rust embedders (like henyey, a Stellar validator) pass already-decoded XDR types directly, avoiding ~45 redundant XDR serialization/deserialization round-trips per transaction.

The draft has the core plumbing working (refactored `invoke_host_function_core()`, new `build_storage_map_from_typed_ledger_entries()`, new `invoke_host_function_typed()`), but there are several gaps that must be filled before it's usable by henyey.

### What Already Works

1. **`invoke_host_function_core()`** — Internal function extracted from the bytes API. Takes typed `Footprint`, `StorageMap`, `HostFunction`, `AccountId`, etc. Creates Host, invokes, returns `(Result<ScVal, HostError>, Storage, Events)`.

2. **`build_storage_map_from_typed_ledger_entries()`** — Builds `StorageMap` + `TtlEntryMap` from `(Rc<LedgerEntry>, Option<Rc<TtlEntry>>)` iterator. Validates TTL liveness, footprint membership, adds missing footprint keys as `None`.

3. **`invoke_host_function_typed()`** — Public entry point. Accepts typed inputs, builds footprint/storage, calls `_core`, returns `InvokeHostFunctionTypedResult { invoke_result, storage, events }`.

4. **`InvokeHostFunctionTypedResult`** — New result struct with typed fields.

5. **One test** — `test_typed_api_matches_xdr_api_for_wasm_upload` verifies the invoke result matches between typed and bytes APIs for a simple WASM upload.

### Gaps to Fill

#### Gap 1: `_ttl_map` is built but discarded

In `invoke_host_function_typed()` (line ~645), `build_storage_map_from_typed_ledger_entries` returns `(storage_map, _ttl_map)` but the `_ttl_map` is thrown away. The `ttl_map` contains `Rc<LedgerKey> → Rc<TtlEntry>` mappings that are needed downstream by embedders for:
- Extracting key hashes for rent computation (`ttl_entry.key_hash`)
- Knowing old `live_until_ledger_seq` values for diffing

**Fix**: Include `ttl_map` in `InvokeHostFunctionTypedResult`. Add a field:
```rust
pub struct InvokeHostFunctionTypedResult {
    pub invoke_result: Result<ScVal, HostError>,
    pub storage: Storage,
    pub events: Events,
    pub ttl_map: TtlEntryMap,  // NEW: initial TTL entries for storage diffing
}
```

The type `TtlEntryMap` is already defined as `type TtlEntryMap = MeteredOrdMap<Rc<LedgerKey>, Rc<TtlEntry>, Budget>;` in `e2e_invoke.rs` line ~36. It needs to be made `pub` (currently it's `pub(crate)` or private — check and fix).

#### Gap 2: `build_storage_footprint_from_xdr` still used for typed path

`invoke_host_function_typed()` accepts `resources: SorobanResources` and then calls `build_storage_footprint_from_xdr(budget, resources.footprint)` which internally iterates `footprint.read_write` and `footprint.read_only` to build a `Footprint`. Since `LedgerFootprint` contains typed `Vec<LedgerKey>` (not bytes), this isn't doing XDR parsing — it's doing `metered_clone` of keys. This is acceptable as-is, but the function name is misleading. Consider renaming or adding a typed variant. **This is low priority — the current approach works fine.**

#### Gap 3: `_restored_keys` is built but never used

In `invoke_host_function_typed()` (line ~642), `_restored_keys` is built via `build_restored_key_set()` but then discarded. The `restored_keys` set is used by `get_ledger_changes()` to zero out `old_entry_size_bytes_for_rent` and `old_live_until_ledger` for restored entries (so rent is charged as if they're new). The typed API doesn't call `get_ledger_changes()`, but the embedder needs this information.

**Fix**: Include `restored_keys` in `InvokeHostFunctionTypedResult`:
```rust
pub restored_keys: Option<RestoredKeySet>,
```

The type `RestoredKeySet` is defined as `type RestoredKeySet = MeteredOrdMap<Rc<LedgerKey>, (), Budget>;` (line ~39). It also needs to be made `pub`.

#### Gap 4: Expose `get_ledger_changes` as a public typed alternative

The most impactful gap. Currently `get_ledger_changes()` is private (module-level `fn`) and returns `Vec<LedgerEntryChange>` with **encoded bytes** (`encoded_key: Vec<u8>`, `encoded_new_value: Option<Vec<u8>>`). It also needs `init_storage_snapshot` (the deep-cloned pre-invocation state).

The typed API was designed so embedders do their own diffing, which is fine. But we should provide a **typed** equivalent of `get_ledger_changes` that operates on typed data instead of bytes, and returns typed results. This function should:

1. Take the post-invocation `Storage`, the initial `TtlEntryMap`, a pre-invocation snapshot (or the initial `StorageMap`), and `restored_keys`
2. Return `Vec<TypedLedgerEntryChange>` with typed fields instead of encoded bytes

Define a new result type:
```rust
/// Typed equivalent of `LedgerEntryChange` — no XDR-encoded fields.
pub struct TypedLedgerEntryChange {
    /// Whether the ledger entry is read-only.
    pub read_only: bool,
    /// The ledger key.
    pub key: Rc<LedgerKey>,
    /// Old entry size for rent (XDR size, or Wasm memory cost for code entries).
    pub old_entry_size_bytes_for_rent: u32,
    /// New entry value (None if deleted or read-only).
    pub new_entry: Option<Rc<LedgerEntry>>,
    /// New entry size for rent.
    pub new_entry_size_bytes_for_rent: u32,
    /// TTL change info.
    pub ttl_change: Option<LedgerEntryLiveUntilChange>,  // reuse existing type
}
```

And a public function:
```rust
pub fn get_ledger_changes_typed(
    budget: &Budget,
    storage: &Storage,
    init_storage_map: &StorageMap,
    init_ttl_entries: TtlEntryMap,
    min_live_until_ledger: u32,
    restored_keys: &Option<RestoredKeySet>,
) -> Result<Vec<TypedLedgerEntryChange>, HostError>
```

This mirrors the logic of the existing `get_ledger_changes()` but:
- Uses `init_storage_map` instead of `SnapshotSource` trait (the init storage map IS the snapshot)
- Returns typed `Rc<LedgerKey>` / `Rc<LedgerEntry>` instead of encoded bytes
- Still computes `entry_size_for_rent` (needs XDR size — use `WriteXdr::to_xdr_len()` or the metered equivalent)

**Important**: This function needs to compute XDR sizes for rent. The existing `get_ledger_changes` calls `metered_write_xdr` to serialize entries to get their size. For the typed version, we still need entry sizes but can use `entry.to_xdr(Limits::none()).len()` or a dedicated size-only computation. The `entry_size_for_rent()` public function takes `(budget, entry, entry_xdr_size)`.

**Alternative**: If adding `get_ledger_changes_typed` is too much scope, just make the existing types and the initial storage map available so the embedder can do this themselves. The minimum viable change is Gap 1 + Gap 3 (expose `ttl_map` and `restored_keys`).

#### Gap 5: Make necessary types `pub`

Several types used in the API are not currently public:
- `TtlEntryMap` (line ~36) — needs `pub` export
- `RestoredKeySet` (line ~39) — needs `pub` export  
- `StorageMap` (in `storage.rs` line 27) — already `pub`
- `FootprintMap` (in `storage.rs` line 25) — already `pub`
- `EntryWithLiveUntil` (in `storage.rs` line 26) — already `pub`

Check which of these are accessible from outside the crate and fix visibility.

#### Gap 6: Comprehensive tests

The current test only covers WASM upload with empty ledger entries. Add tests that exercise:

1. **Contract creation** — Tests footprint handling with both read and read-write entries
2. **Contract invocation with storage ops** — Tests that storage modifications (put/get/del) produce correct post-invocation `Storage`. Compare the typed result's `storage.map` entries against the bytes API's `ledger_changes`.
3. **TTL extension** — Tests that live_until_ledger values are correctly updated in the returned Storage
4. **Entry restoration (autorestore)** — Tests with `restored_rw_entry_indices` to verify restored entries appear correctly in Storage
5. **Error cases** — Budget exhaustion, missing footprint entries
6. **Events parity** — Verify the typed API's `events` field contains the same events as the bytes API's `encoded_contract_events` (after decoding)

For each test, invoke both `invoke_host_function_helper` (bytes) and `invoke_host_function_typed_helper` (typed) with the same inputs, and assert equivalence of:
- `invoke_result` (already done in the existing test)
- Storage state (diff the bytes API's `ledger_changes` against the typed API's `storage.map`)
- Events (decode bytes API events and compare with typed API events)
- Budget consumption (should be similar — typed API does less metered XDR work, so CPU will differ slightly, but the result values should match)

Use the existing test helpers and WASM modules already in the test file (ADD_I32, CONTRACT_STORAGE, etc.).

### Files to Edit

- **`soroban-env-host/src/e2e_invoke.rs`** — Main file. All gaps are here.
- **`soroban-env-host/src/test/e2e_tests.rs`** — Test file. Gap 6 is here.

### Build & Test Commands

```bash
# Build (from repo root)
cargo build -p soroban-env-host

# Run all e2e tests
cargo test -p soroban-env-host --test e2e_tests

# Run just the typed API tests (once you name them with a prefix)
cargo test -p soroban-env-host test_typed_api

# Run clippy
cargo clippy -p soroban-env-host
```

### Priority Order

1. **Gap 1** (expose `ttl_map`) — Trivial, 5 min
2. **Gap 3** (expose `restored_keys`) — Trivial, 5 min  
3. **Gap 5** (pub visibility) — Trivial, 5 min
4. **Gap 6** (tests) — Medium effort, 30-60 min. Write these BEFORE Gap 4 to establish the parity baseline.
5. **Gap 4** (typed `get_ledger_changes`) — Largest effort, 30-60 min. This is optional if henyey implements its own diffing, but providing it upstream makes the API much more useful.
6. **Gap 2** (rename footprint builder) — Optional cleanup, 5 min

### Non-Goals

- Do NOT change the existing `invoke_host_function()` bytes API behavior
- Do NOT change the `recording_mode` API
- Do NOT add features to the typed API that don't exist in the bytes API
- Do NOT break any existing tests
