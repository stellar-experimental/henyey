# Proposed Issue: Add typed (non-XDR-encoded) `invoke_host_function` API

**Repository**: `stellar/rs-soroban-env`
**Component**: `soroban-env-host/src/e2e_invoke.rs`

---

## Summary

The current `invoke_host_function()` API in `e2e_invoke` accepts all inputs as
XDR-encoded `&[u8]` and returns all outputs as `Vec<u8>`. This design was
necessary for the C++ stellar-core FFI boundary, but imposes unnecessary
overhead on pure-Rust embedders that already have typed XDR objects.

This issue proposes adding a parallel typed API that accepts and returns native
Rust XDR types directly, eliminating redundant serialization/deserialization
round-trips for Rust-native consumers.

## Background

### Current API signature

```rust
pub fn invoke_host_function<T: AsRef<[u8]>, I: ExactSizeIterator<Item = T>>(
    budget: &Budget,
    enable_diagnostics: bool,
    encoded_host_fn: T,          // HostFunction as XDR bytes
    encoded_resources: T,        // SorobanResources as XDR bytes
    restored_rw_entry_indices: &[u32],
    encoded_source_account: T,   // AccountId as XDR bytes
    encoded_auth_entries: I,     // Vec<SorobanAuthorizationEntry> as XDR bytes
    ledger_info: LedgerInfo,
    encoded_ledger_entries: I,   // Vec<LedgerEntry> as XDR bytes
    encoded_ttl_entries: I,      // Vec<TtlEntry> as XDR bytes
    base_prng_seed: T,
    diagnostic_events: &mut Vec<DiagnosticEvent>,
    trace_hook: Option<TraceHook>,
    module_cache: Option<ModuleCache>,
) -> Result<InvokeHostFunctionResult, HostError>
```

### Current result type

```rust
pub struct InvokeHostFunctionResult {
    pub encoded_invoke_result: Result<Vec<u8>, HostError>,  // ScVal as XDR bytes
    pub ledger_changes: Vec<LedgerEntryChange>,             // keys/entries as XDR bytes
    pub encoded_contract_events: Vec<Vec<u8>>,              // ContractEvent as XDR bytes
}

pub struct LedgerEntryChange {
    pub read_only: bool,
    pub encoded_key: Vec<u8>,                    // LedgerKey as XDR bytes
    pub old_entry_size_bytes_for_rent: u32,
    pub encoded_new_value: Option<Vec<u8>>,      // LedgerEntry as XDR bytes
    pub new_entry_size_bytes_for_rent: u32,
    pub ttl_change: Option<LedgerEntryLiveUntilChange>,
}
```

### What happens inside

The function internally:

1. **Deserializes all inputs** via `metered_from_xdr`:
   - `SorobanResources`, `HostFunction`, `AccountId`, each `SorobanAuthorizationEntry`,
     each `LedgerEntry`, each `TtlEntry`
2. **Builds typed structures**: `Footprint`, `StorageMap`, `Storage`
3. **Deep-clones the entire storage map** (`init_storage_map = storage_map.metered_clone()`)
   to enable post-execution diffing
4. **Constructs a `Host`** via `Host::with_storage_and_budget(storage, budget)`
5. **Executes** via `host.invoke_function(host_function)` — returns typed `ScVal`
6. **Calls `host.try_finish()`** — returns typed `(Storage, Events)`
7. **Re-serializes all outputs** in `get_ledger_changes()`:
   - Each key via `metered_write_xdr(key)` → `encoded_key: Vec<u8>`
   - Each old entry via `metered_write_xdr(old_entry)` (for rent size)
   - Each new entry via `metered_write_xdr(entry)` → `encoded_new_value: Vec<u8>`
   - SHA256 hash of encoded key bytes
8. **Re-serializes events** via `encode_contract_events()`
9. **Re-serializes return value** via `metered_write_xdr(&res)`

For a typical transaction with a 10-entry footprint, this is approximately:
- ~25 input deserializations
- 1 full deep clone of the storage map
- ~25 output re-serializations
- **Total: ~50 XDR encode/decode operations + 1 deep clone**

A Rust-native embedder that already has typed XDR objects (from the same
`stellar-xdr` crate) would need to:
- Serialize its typed objects to bytes → pass to `invoke_host_function` → which
  immediately deserializes them back to the same types
- Receive bytes back → deserialize them to typed objects for further processing

This is a double round-trip (typed→bytes→typed) on both input and output paths.

## Proposal

### Option A: Typed wrapper (minimal change)

Add a new public function that wraps the existing internal logic but accepts and
returns typed objects:

```rust
pub fn invoke_host_function_typed(
    budget: &Budget,
    enable_diagnostics: bool,
    host_function: HostFunction,
    resources: SorobanResources,
    restored_rw_entry_indices: &[u32],
    source_account: AccountId,
    auth_entries: Vec<SorobanAuthorizationEntry>,
    ledger_info: LedgerInfo,
    ledger_entries: Vec<(LedgerEntry, Option<TtlEntry>)>,
    base_prng_seed: [u8; 32],
    diagnostic_events: &mut Vec<DiagnosticEvent>,
    trace_hook: Option<TraceHook>,
    module_cache: Option<ModuleCache>,
) -> Result<InvokeHostFunctionTypedResult, HostError>
```

With a typed result:

```rust
pub struct InvokeHostFunctionTypedResult {
    pub invoke_result: Result<ScVal, HostError>,
    pub storage: Storage,        // The post-execution storage (typed)
    pub events: Events,          // The typed events
    pub contract_events_and_return_value_size: u32,
}
```

This would let the embedder:
1. Skip all input serialization (pass typed objects directly)
2. Skip all output deserialization (receive typed objects directly)
3. Implement their own diffing logic using the returned `Storage` vs their
   initial state (or provide a helper `get_ledger_changes_typed()`)

### Option B: Make `get_ledger_changes` public + typed result struct

A less invasive option: keep the existing `invoke_host_function` unchanged but:

1. Make `get_ledger_changes()` public (currently module-private)
2. Add a typed `LedgerEntryChangeTyped` struct that holds `Rc<LedgerKey>` and
   `Option<Rc<LedgerEntry>>` instead of `Vec<u8>`
3. Add a `get_ledger_changes_typed()` variant that returns the typed struct

This lets embedders construct `Storage` themselves (using the existing public
`Host::with_storage_and_budget`, `invoke_function`, `try_finish`) and then call
the public typed diff function.

### Option C: Accept `Storage` directly (most flexible)

The most flexible approach: allow embedders to provide a pre-built `Storage`
object instead of encoded entries:

```rust
pub fn invoke_host_function_with_storage(
    budget: &Budget,
    enable_diagnostics: bool,
    host_function: HostFunction,
    source_account: AccountId,
    auth_entries: Vec<SorobanAuthorizationEntry>,
    ledger_info: LedgerInfo,
    storage: Storage,
    base_prng_seed: [u8; 32],
    diagnostic_events: &mut Vec<DiagnosticEvent>,
    module_cache: Option<ModuleCache>,
) -> Result<InvokeHostFunctionTypedResult, HostError>
```

This eliminates the `build_storage_map_from_xdr_ledger_entries` step entirely
and gives the embedder full control over storage construction.

## Motivation

### Primary: Performance for Rust-native embedders

Any Rust application that embeds soroban-env-host and already works with typed
`stellar-xdr` objects (e.g., a Rust reimplementation of the Stellar validator,
or a Rust-based transaction simulation service) currently pays for two full
XDR serialization round-trips per transaction — one on input, one on output.

Profiling shows XDR encode/decode overhead is approximately 45 microseconds
per transaction on typical mainnet workloads (~10-entry footprint). While the
WASM execution itself dominates (~360 microseconds), the XDR overhead is
non-trivial at scale (130+ transactions per ledger = ~6 milliseconds per
ledger just for XDR round-trips that could be eliminated).

More significantly, the `init_storage_map.metered_clone()` at line 438 deep-clones
the entire storage map before execution solely to enable `get_ledger_changes()`
diffing. A Rust-native embedder that maintains its own pre-execution state
snapshot doesn't need this clone at all.

### Secondary: Code simplification for embedders

Embedders currently need:
- Helper functions to serialize each input type to `Vec<u8>`
- Helper functions to deserialize each output `Vec<u8>` back to typed objects
- Special handling for the `encoded_key` / `encoded_new_value` fields in
  `LedgerEntryChange`

A typed API would eliminate all of this boilerplate.

### Compatibility

The existing `invoke_host_function` (bytes-based) API would remain unchanged
for backward compatibility. stellar-core's C++ FFI path would continue using
the bytes API. The typed API would be an additional public surface.

## Implementation Notes

The internal implementation already works with typed objects — the current
function deserializes inputs immediately, works with types throughout, and
only re-serializes at the end. The typed API would essentially expose the
internal typed flow directly, skipping the encode/decode bookends.

Key internal functions that already operate on typed data:
- `build_storage_footprint_from_xdr()` → could become
  `build_storage_footprint()` accepting `LedgerFootprint` directly
- `build_storage_map_from_xdr_ledger_entries()` → already builds a typed
  `StorageMap`
- `Host::with_storage_and_budget()` — public, accepts typed `Storage`
- `Host::invoke_function()` — public, accepts typed `HostFunction`
- `Host::try_finish()` — public, returns typed `(Storage, Events)`
- `get_ledger_changes()` — private, operates on typed `Storage` but outputs
  `Vec<u8>` encoded results

The main work would be:
1. Factor out the typed core from `invoke_host_function` into a shared
   implementation
2. Create the typed result structs
3. Optionally create a typed `get_ledger_changes` variant
