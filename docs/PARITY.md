# Parity with stellar-core

henyey is a Rust reimplementation of stellar-core. "Parity" here does **not**
mean a line-by-line or structure-by-structure copy of the C++. It means henyey
behaves identically to stellar-core *where it is observable*, while remaining
free to differ everywhere it is not.

This document is the single source of truth for what parity requires. Other
docs (`CLAUDE.md`, `AGENTS.md`, `README.md`) and the project-loop skills
(`.claude/skills/`) reference it instead of restating the definition.

## The two hard requirements (MUST hold)

1. **Observed-behavior parity** — henyey produces bit-for-bit identical output
   to stellar-core on the observable surface enumerated below.
2. **Interoperability** — a henyey node can join a network alongside
   stellar-core nodes and reach identical consensus and ledger state, and any
   external consumer (peer, Horizon, stellar-rpc, archive reader) cannot
   distinguish it from a core node by the bytes it emits.

Reference is the pinned `stellar-core/` submodule. Protocol support is 24+.

## Observable / interop surface — MUST match

| Surface | Owning crates |
|---|---|
| Ledger header & hashes (`LedgerHeader`, ledger hash, bucket-list hash) | ledger, bucket |
| BucketList ordering, merge results, bucket hashes, archived bucket bytes | bucket |
| Transaction results (`TransactionResult` codes & XDR) | tx, ledger |
| Transaction/ledger meta (`LedgerCloseMeta` / `TransactionMeta` XDR) | ledger, tx, app |
| Classic & Soroban event XDR + ordering | tx |
| SCP wire (`SCPEnvelope`/`SCPStatement` signed bytes, nomination/balloting sequencing, quorum decisions) | scp, herder |
| Overlay P2P wire (`StellarMessage` framing, auth handshake, flood/flow-control observable to peers) | overlay, herder |
| History archive format (checkpoint layout, `.xdr` framing, HAS files, file naming) | history, historywork |
| HTTP API contract (`/info`, `/tx`, `/getledgerentry`, upgrades, surveys) | app |
| JSON-RPC contract (method request/response schemas) | rpc |
| CLI contract (subcommands consumed by external tooling) | henyey |
| Crypto outputs (signatures, hashes, strkey encodings) | crypto |

**Rule of thumb:** *if a core node, Horizon, stellar-rpc, an archive consumer,
or a peer could tell henyey apart by observing bytes on a wire, on disk in an
archive, or in an API response — it must NOT differ.*

## Divergeable surface — MAY differ freely

The following are **not** parity concerns. Changes here are subject to the
normal correctness / scope / risk review, but they are never flagged as parity
violations:

- **Internal architecture** — module/crate boundaries, type layout, the
  threading/async model, savepoint vs. nested-`LedgerTxn` strategy, SQLite-only
  storage, and any other implementation structure.
- **Helper utilities** — internal helpers and abstractions with no upstream
  counterpart, as long as their observable output matches.
- **Metrics** — metric names, labels, existence, and values.
- **Logging** — log formats, levels, and content.
- **Admin / debug endpoints** not consumed by core, Horizon, stellar-rpc, or
  peers.
- **Performance** — optimizations of any kind, provided the observable surface
  and interop are preserved.

## Mapping to `PARITY_STATUS.md`

Each crate's `PARITY_STATUS.md` already encodes these tiers:

- **Intentional Omissions** — parts of the divergeable surface deliberately not
  built. Excluded from the parity %. (If something listed here is actually part
  of the observable/interop contract, it is a real gap, not an omission.)
- **Architectural Differences** — divergeable-surface differences with a
  rationale. Expected, and never counted as gaps.

Parity % (`implemented / (implemented + gaps)`) measures **observable-surface
coverage** against upstream — not internal sameness. An internal helper
implemented differently but producing the same observable output is not a gap;
it belongs under Architectural Differences.
