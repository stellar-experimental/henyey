# Issue: Add `WriteXdr::xdr_len()` for Zero-Allocation Size Computation

## Summary

Add an `xdr_len(&self) -> usize` method to the `WriteXdr` trait that computes the serialized XDR byte length without allocating memory or performing I/O. This enables callers that only need the size (not the bytes) to avoid unnecessary heap allocations and serialization overhead.

## Motivation

Several downstream consumers of `rs-stellar-xdr` serialize XDR values **only to measure their byte length**, then discard the bytes. The most prominent example is `soroban-env-host`'s rent computation in `e2e_invoke.rs`, which does this per ledger entry:

```rust
let mut buf = vec![];
metered_write_xdr(budget, old_entry.as_ref(), &mut buf)?;
// buf contents are never read — only buf.len() is used:
let size = entry_size_for_rent(budget, &old_entry, buf.len() as u32)?;
```

This pattern appears ~4 times per footprint entry in `get_ledger_changes()` / `get_ledger_changes_typed()`. For a typical Soroban transaction with ~10 footprint entries, that's ~40 unnecessary `Vec<u8>` allocations and full serialization passes per transaction, just to get a length.

In the context of the Stellar network, this matters: every microsecond in the ledger close path affects validator performance. The henyey validator project measured ~303ms mean ledger close time and is working to reduce it toward stellar-core's ~168ms. Eliminating unnecessary allocations and serialization in the hot path contributes to that goal.

### Prior art: C++ `xdrpp` already has this

The C++ XDR library used by stellar-core (`lib/xdrpp`) has provided zero-allocation size computation since its inception via `xdr::xdr_size()`:

```cpp
// lib/xdrpp/xdrpp/types.h:222
template<typename T> std::size_t
xdr_size(const T&t) {
  return xdr_traits<T>::serial_size(t);
}
```

Each XDR type has a `serial_size()` specialization that recursively sums field sizes without allocating or serializing. stellar-core uses this extensively in its C++ code paths — `ExtendFootprintTTLOpFrame`, `RestoreFootprintOpFrame`, and `InMemorySorobanState` all compute entry sizes this way.

The Rust `rs-stellar-xdr` library is simply missing this counterpart. The proposed `xdr_len()` brings the Rust library to parity with what the C++ library has always had.

### Impact on stellar-core

This benefits stellar-core's own Rust code (soroban-env-host), not just external consumers:

- **Old entry sizes in `get_ledger_changes()`**: The "before" snapshot entries are serialized purely to measure their byte length, then the buffer is dropped. An `xdr_len()` method eliminates this allocation entirely.
- **New/modified entry sizes**: These serializations serve double duty (the bytes are returned as `encoded_new_value`), so `xdr_len()` doesn't help here in the bytes-based API. However, the new typed API (`invoke_host_function_typed` / `get_ledger_changes_typed`) doesn't produce encoded bytes at all, so ALL size computations there would benefit.
- **Simulation code** (`soroban-simulation/src/resources.rs`): Calls `entry.to_xdr(limits)?.len()` for size computation — would benefit directly.

## Proposed Change

Add a method to the `WriteXdr` trait:

```rust
pub trait WriteXdr {
    #[cfg(feature = "std")]
    fn write_xdr<W: Write>(&self, w: &mut Limited<W>) -> Result<(), Error>;

    /// Compute the exact serialized XDR byte length without allocating or writing.
    ///
    /// This is equivalent to `self.to_xdr(Limits::none()).unwrap().len()` but
    /// performs no heap allocation and no I/O — only arithmetic.
    fn xdr_len(&self) -> usize;

    // ...existing methods unchanged...
}
```

### Implementation for each XDR type category

The code generator already walks the type tree to produce `write_xdr` implementations. Generating `xdr_len` is strictly simpler — it mirrors `write_xdr` structurally but returns `usize` sums instead of writing bytes.

**Fixed-size primitives** (u32, i32, u64, i64, bool):
```rust
fn xdr_len(&self) -> usize { 4 }  // u32, i32, bool
fn xdr_len(&self) -> usize { 8 }  // u64, i64
```

**Fixed-size byte arrays** (Hash = `[u8; 32]`, Uint256 = `[u8; 32]`):
```rust
fn xdr_len(&self) -> usize { 32 }  // always padded to multiple of 4 already
```

**Structs** (sum of fields):
```rust
// For LedgerEntry { last_modified_ledger_seq, data, ext }
fn xdr_len(&self) -> usize {
    self.last_modified_ledger_seq.xdr_len()
    + self.data.xdr_len()
    + self.ext.xdr_len()
}
```

**Unions/enums** (discriminant + active variant):
```rust
// For LedgerEntryData
fn xdr_len(&self) -> usize {
    4 + match self {  // 4-byte discriminant
        Self::Account(v) => v.xdr_len(),
        Self::Trustline(v) => v.xdr_len(),
        Self::ContractData(v) => v.xdr_len(),
        Self::ContractCode(v) => v.xdr_len(),
        // ...
    }
}
```

**Variable-length bytes** (`VecM<u8, MAX>`, `BytesM<MAX>`, `StringM<MAX>`):
```rust
fn xdr_len(&self) -> usize {
    4 + self.len() + pad_len(self.len())  // length prefix + data + XDR padding
}
```

**Variable-length arrays** (`VecM<T, MAX>` where T: WriteXdr):
```rust
fn xdr_len(&self) -> usize {
    4 + self.iter().map(|t| t.xdr_len()).sum::<usize>()  // length prefix + elements
}
```

**Optional** (`Option<T>`):
```rust
fn xdr_len(&self) -> usize {
    4 + match self {  // 4-byte flag
        Some(v) => v.xdr_len(),
        None => 0,
    }
}
```

**Void**:
```rust
fn xdr_len(&self) -> usize { 0 }
```

### Code generator changes

The `rs-stellar-xdr` types are generated from `.x` (XDR IDL) files. The generator that produces `write_xdr` implementations would need a parallel codepath to produce `xdr_len` implementations. The transformation is mechanical:

| `write_xdr` pattern | `xdr_len` equivalent |
|---|---|
| `field.write_xdr(w)?;` | `field.xdr_len()` (sum) |
| `discriminant.write_xdr(w)?;` | `4 +` |
| `len.write_xdr(w)?;` | `4 +` |
| `w.write_all(&self.0)?;` | `self.len()` |
| `w.write_all(&[0u8; 3][..padding])?;` | `pad_len(self.len())` |

### Properties

- **Infallible**: `xdr_len()` cannot fail — it's pure arithmetic on in-memory values. It returns `usize` (not `Result`), unlike `write_xdr` which can fail on I/O or limits. This is a deliberate simplification: since we're not doing I/O and not enforcing write limits, there's no error path.
- **No `std` gate**: `xdr_len()` doesn't need `std` — it has no I/O dependency. This makes it usable in `no_std` environments.
- **Deterministic**: XDR encoding is canonical, so the length is always deterministic for a given value.
- **Consistent**: `xdr_len()` MUST equal `to_xdr(Limits::none()).unwrap().len()` for all values. This invariant should be tested.

## Alternatives Considered

### Counting Writer (no upstream change)

A `Write` implementation that counts bytes without storing them:

```rust
struct CountingWriter(usize);
impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
```

This works today with the existing `write_xdr` API and eliminates the heap allocation. However, it still executes the full serialization logic (field traversal, `Limited` depth/length tracking, padding byte writes to the counting sink). A native `xdr_len()` avoids all of that overhead.

**This is a viable short-term workaround** that consumers can implement today without waiting for an upstream change.

### Caching XDR size on the type

Some systems cache serialized sizes. This isn't appropriate here because XDR types are plain data structs that can be mutated; a cached size would go stale.

## Testing

For every type that implements `WriteXdr`, add a property: `xdr_len() == to_xdr(Limits::none()).unwrap().len()`. This can be tested exhaustively for simple types and with representative values for complex types.

The existing test infrastructure in `rs-stellar-xdr` can be extended with round-trip length checks alongside the existing round-trip serialization checks.
