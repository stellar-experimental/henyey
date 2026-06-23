---
name: xdr-eval
description: Audit a crate for raw byte arrays that should use rs-stellar-xdr types
argument-hint: <crate-path> [--apply]
---

Parse `$ARGUMENTS`:
- The first argument is the crate path. Replace `$TARGET` with it.
- If `--apply` is present, set `$MODE = apply`. Otherwise set `$MODE = review`.

# XDR Type Audit

Audit the Rust crate at `$TARGET` to find places where `rs-stellar-xdr`
(`stellar_xdr::curr`) types should be used instead of custom types, raw byte
arrays, or conversion boilerplate.

## Guiding Principle

The codebase should prefer types from `rs-stellar-xdr` over defining new
types that duplicate them. Every custom wrapper or raw `[u8; N]` that stands
in for an XDR type adds cognitive overhead and conversion boilerplate.
Exceptions exist (see Legitimate Uses below), but each must be justified.

## Mode

- **`$MODE = review`** (default): Produce a ranked list of findings with
  file:line references. Do NOT make any changes.
- **`$MODE = apply`**: Perform the replacements directly. For each change,
  briefly state what you changed and why. Run `cargo clippy -p <crate>` and
  `cargo test -p <crate>` after each logical group of changes to verify
  correctness.

## Categories

For each finding, classify it into exactly one category:

### RAW_BYTES — Raw byte arrays standing in for XDR types

`[u8; 32]` used where `Hash`, `AccountId`, `NodeId`, `PoolId`, or
`ClaimableBalanceId` would be appropriate. Common symptoms:

- `HashMap<[u8; 32], ...>` or `HashSet<[u8; 32]>` where the bytes represent
  a known XDR type.
- `EntryStore<[u8; 32], ...>` key type parameters.
- Struct fields typed `[u8; 32]` that are populated from `.0` access on an
  XDR wrapper.
- Helper functions like `account_id_to_bytes()` or `hash_to_key()` that
  extract `.0` for map lookups.

### WRAPPER_TYPE — Newtype wrapper duplicating an XDR type

A custom `struct Foo([u8; 32])` or `struct Foo(pub [u8; N])` that is
functionally identical to an existing XDR type. Look for:

- Bidirectional `From` / `Into` impls between the wrapper and the XDR type.
- The wrapper adding only utility methods (hashing, hex encoding, display)
  that could be provided via an extension trait on the XDR type instead.

### MIRROR_ENUM — Custom enum mirroring an XDR enum

A locally defined enum whose variants correspond 1:1 (or nearly so) to an
XDR enum, with conversion impls between them. For example, a custom
`AssetKey` that decomposes `TrustLineAsset` into raw byte fields.

### CONVERSION_BOILERPLATE — Unnecessary From/Into/TryFrom impls

`From<XdrType> for CustomType` and the reverse, where the custom type could
be eliminated entirely. Count the number of conversion impl blocks as a
measure of the overhead.

### DUPLICATE_HELPER — Duplicated conversion functions

The same conversion logic (e.g., `AccountId` to `[u8; 32]`) defined in
multiple places across the crate or workspace. List all locations.

## Legitimate Uses (Do NOT Flag)

The following are acceptable and should not be reported:

- **Crypto wrappers**: Types wrapping `ed25519_dalek` / `x25519_dalek`
  primitives (e.g., `PublicKey`, `SecretKey`, `Signature` in `crates/crypto`)
  that add signing/verification behavior. These wrap crypto library types,
  not XDR types.
- **Extension traits**: Traits that add methods to XDR types without wrapping
  them.
- **Composite keys**: Tuple types combining multiple XDR types for use as
  map keys (e.g., `(AccountId, TrustLineAsset)`) are fine — the concern is
  when those components are decomposed into raw bytes.
- **Performance-critical paths**: If there is a documented, measured
  performance reason for using raw bytes (e.g., avoiding XDR serialization
  overhead in a hot loop), note it as an exception.

## Analysis Process

1. **Identify XDR imports**: Find all `use stellar_xdr::curr::*` statements
   and the XDR types actually used in the crate.
2. **Scan for raw byte arrays**: Search for `[u8; 32]`, `[u8; 4]`,
   `[u8; 12]`, `[u8; 56]` in struct fields, type aliases, and generic
   parameters. For each, determine whether an XDR type exists for that data.
3. **Scan for wrapper structs**: Find structs with a single field that is a
   byte array and have `From`/`Into` impls to XDR types.
4. **Scan for mirror enums**: Find enums with `From`/`Into` impls to XDR
   enum types.
5. **Scan for conversion helpers**: Find functions named `*_to_bytes`,
   `*_to_key`, `*_from_bytes`, or that contain `.0` access on XDR types
   for the purpose of key extraction.
6. **Cross-reference duplicates**: Check if the same conversion exists in
   multiple files across the workspace.

## Scope

- Audit `$TARGET/src/` (production and test code).
- Ignore `stellar-core/`.
- When checking for duplicates (DUPLICATE_HELPER), scan the full `crates/`
  workspace.

## Ranking

Rank findings by impact:
- **High**: Pervasive pattern (used in 10+ places) or causes significant
  boilerplate / duplication.
- **Medium**: Used in 3-9 places or adds moderate cognitive overhead.
- **Low**: Isolated occurrence or minor style issue.

## Output Format (review mode)

Per finding:

```
### [RANK]. [CATEGORY] — one-line summary
- **Location**: file:line (and file:line if duplicated)
- **Current**: what the code does now
- **Proposed**: the XDR type that should replace it
- **Impact**: number of call sites / conversions that would be eliminated
- **Effort**: Low / Medium / High — estimated difficulty of the replacement
```

End with a summary table:

```
## Summary

| Category | Count | High | Medium | Low |
|----------|-------|------|--------|-----|
| RAW_BYTES | ... | ... | ... | ... |
| WRAPPER_TYPE | ... | ... | ... | ... |
| MIRROR_ENUM | ... | ... | ... | ... |
| CONVERSION_BOILERPLATE | ... | ... | ... | ... |
| DUPLICATE_HELPER | ... | ... | ... | ... |
```

## Apply Mode Guidelines

When `$MODE = apply`:
- Work through findings in rank order (highest impact first).
- Make one logical change at a time — do not batch unrelated replacements.
- When replacing a key type (e.g., `[u8; 32]` to `Hash`), update all
  associated code: the collection type, insertion sites, lookup sites, and
  remove the now-unnecessary conversion helpers.
- After each change, verify with `cargo clippy -p <crate>` and
  `cargo test -p <crate>`.
- If a change breaks tests or introduces warnings, revert and move on.
- When eliminating a wrapper type, check for workspace-wide usage before
  removing the definition.
- Stop and report if a change would alter observable behavior.
