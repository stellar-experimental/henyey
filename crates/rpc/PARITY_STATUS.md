# stellar-core Parity Status

**Crate**: `henyey-rpc`
**Upstream**: `No direct stellar-core source equivalent`
**Overall Parity**: 100%
**Last Updated**: 2026-04-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| JSON-RPC transport | Full | Request parsing, body limit, batch rejection |
| Method dispatch | Full | All scoped RPC methods routed |
| Health and metadata | Full | Health, network, version, latest ledger |
| Fee statistics | Full | Sliding windows and percentile responses |
| Ledger queries | Full | Entries, ledgers, transactions, ranges |
| Event queries | Full | Filters, pagination, topic formatting |
| Transaction submission | Full | Queue submission and error-result XDR |
| Soroban simulation | Full | Invoke, extend TTL, restore, auth, fees |
| Embedded node integration | Full | Direct `App`, DB, and bucket access |

`henyey-rpc` has no direct stellar-core source directory. It provides a native
Stellar JSON-RPC surface for henyey nodes, replacing the standalone
stellar-rpc/captive-core deployment model with in-process access to henyey
application state.

## File Mapping

| Scoped Component | Rust Module | Notes |
|------------------|-------------|-------|
| JSON-RPC envelopes | `src/types/jsonrpc.rs`, `src/error.rs` | Request, response, and error payloads |
| HTTP server | `src/server.rs` | Axum listener, graceful shutdown, body limit |
| Request context | `src/context.rs` | App abstraction, semaphores, request timeout |
| Dispatch | `src/dispatch.rs` | Method-name routing |
| Fee windows | `src/fee_window.rs`, `src/methods/fee_stats.rs` | Percentiles and fee response formatting |
| Health and metadata | `src/methods/health.rs`, `network.rs`, `latest_ledger.rs`, `version_info.rs` | Network and node status methods |
| Ledger entry lookup | `src/methods/get_ledger_entries.rs` | Bucket snapshot reads and TTL lookup |
| Transaction lookup | `src/methods/get_transaction.rs`, `get_transactions.rs`, `transaction_response.rs` | Single and paginated transaction responses |
| Ledger range lookup | `src/methods/get_ledgers.rs` | Ledger pagination |
| Event lookup | `src/methods/get_events.rs` | Event filters and output formatting |
| Transaction submission | `src/methods/send_transaction.rs` | Envelope decode, hash, queue status mapping |
| Simulation handler | `src/simulate/mod.rs`, `preflight.rs` | Soroban preflight execution |
| Simulation resources | `src/simulate/resources.rs`, `response.rs`, `convert.rs` | Resource/fee calculation and XDR conversion |
| Simulation snapshot | `src/simulate/snapshot.rs` | Bucket-list-backed host snapshot |
| Shared utilities | `src/util.rs` | XDR, pagination, TOID, timestamp, TTL helpers |

## Component Mapping

### JSON-RPC transport (`server.rs`, `types/jsonrpc.rs`, `error.rs`, `dispatch.rs`)

Corresponds to: scoped Stellar JSON-RPC server surface.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Request envelope parsing | `JsonRpcRequest` | Full |
| Response/error envelope formatting | `JsonRpcResponse`, `JsonRpcError` | Full |
| HTTP body limit and batch rejection | `rpc_handler()` | Full |
| Method dispatch | `dispatch()` | Full |
| Request timeout and overload handling | `RpcContext` semaphores | Full |

### Embedded node context (`context.rs`, `server.rs`)

Corresponds to: henyey in-process node integration.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| App abstraction for handlers | `RpcAppHandle` | Full |
| Shared handler context | `RpcContext` | Full |
| Graceful server lifecycle | `RpcServer`, `RpcServerRunning` | Full |
| Fee-window background ingestion | `fee_window_poller()` | Full |

### Health and metadata methods (`methods/*.rs`)

Corresponds to: scoped read-only RPC metadata methods.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| `getHealth` | `methods::health::handle()` | Full |
| `getNetwork` | `methods::network::handle()` | Full |
| `getLatestLedger` | `methods::latest_ledger::handle()` | Full |
| `getVersionInfo` | `methods::version_info::handle()` | Full |

### Fee statistics (`fee_window.rs`, `methods/fee_stats.rs`)

Corresponds to: scoped `getFeeStats` method.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Sliding fee windows | `FeeWindows` | Full |
| Nearest-rank percentile calculation | `compute_fee_distribution()` | Full |
| JSON response formatting | `fee_stats::handle()` | Full |

### Ledger, transaction, and event queries (`methods/`, `util.rs`)

Corresponds to: scoped data query RPC methods.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| `getLedgerEntries` | `get_ledger_entries::handle()` | Full | Henyey deduplicates keys by decoded `LedgerKey` before lookup — duplicate keys in the request produce a single response entry. Upstream Soroban RPC does not deduplicate. This is an intentional hardening divergence that only affects adversarial/malformed requests. |
| `getTransaction` | `get_transaction::handle()` | Full |
| `getTransactions` | `get_transactions::handle()` | Full | Henyey adds a 10 MiB DB load budget (`max_tx_load_bytes`) that may return fewer transactions than the requested limit. Not present in stellar-core RPC. |
| `getLedgers` | `get_ledgers::handle()` | Full | Henyey adds a 10 MiB DB load budget (`max_ledger_meta_load_bytes`) that may return fewer ledgers than the requested limit. Not present in stellar-core RPC. |
| `getEvents` filtering and pagination | `get_events::handle()` | Full | Henyey adds a 10 MiB DB load budget (`max_event_load_bytes`) that may return fewer events than the requested limit. Not present in stellar-core RPC. |
| Shared XDR/pagination/TOID helpers | `util.rs` | Full |

### Transaction submission (`methods/send_transaction.rs`)

Corresponds to: scoped `sendTransaction` method.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Envelope decode and transaction hash | `send_transaction::handle()` | Full |
| Herder queue submission | `RpcAppHandle::submit_transaction()` | Full |
| Submission status and error-result response | `build_error_result()` | Full |

### Soroban simulation (`simulate/`)

Corresponds to: scoped `simulateTransaction` method.

| Scoped capability | Rust | Status |
|-------------------|------|--------|
| Operation extraction and validation | `simulate::handle()` | Full |
| Host preflight execution | `simulate::preflight` | Full |
| Resource and fee adjustment | `simulate::resources` | Full |
| Bucket-list snapshot reads | `BucketListSnapshotSource` | Full |
| Response and state-change serialization | `simulate::response` | Full |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| Captive-core process management | Henyey is the node; RPC reads in-process state directly |
| Standalone ingestion pipeline | Ledger-close metadata ingestion lives in `henyey-app` and `henyey-db` |
| Alternate database backends | SQLite is the only supported henyey backend |
| Prometheus metrics endpoint | Operational metrics are outside the scoped RPC method surface |
| General HTTP middleware such as CORS | Deployment model is embedded/node-local by default |

## Gaps

No known gaps.

## Architectural Differences

1. **Embedded service model**
   - **stellar-core**: No native JSON-RPC subsystem; Stellar RPC is a standalone service around captive core.
   - **Rust**: `henyey-rpc` is embedded directly in the henyey node process.
   - **Rationale**: Direct app access removes IPC and keeps responses aligned with live validator state.

2. **State access**
   - **stellar-core**: RPC-style queries rely on an external ingestion database.
   - **Rust**: Queries use `henyey-db` plus live bucket snapshots from `henyey-bucket`.
   - **Rationale**: Reuses in-process state and avoids duplicate ingestion plumbing.

3. **Soroban simulation**
   - **stellar-core**: Simulation is not exposed through a stellar-core JSON-RPC module.
   - **Rust**: The RPC crate calls `soroban-env-host-p25` inside controlled blocking sections.
   - **Rationale**: Native host integration keeps preflight deterministic and avoids captive-core orchestration.

4. **Concurrency control**
   - **stellar-core**: No equivalent RPC admission-control surface.
   - **Rust**: Request, simulation, DB, and bucket I/O semaphores bound expensive work.
   - **Rationale**: Protects the embedded node from RPC resource starvation.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| JSON-RPC transport/context/errors | No direct equivalent | 34 `#[test]` / `#[tokio::test]` | Body limits, version checks, semaphores, errors |
| Shared utilities and fee windows | No direct equivalent | 47 `#[test]` | XDR, TOID, pagination, TTL, fees |
| Query and submission methods | No direct equivalent | 19 `#[test]` | Events, ledgers, send-transaction helpers |
| Soroban simulation | No direct equivalent | 62 `#[test]` | Preflight, resources, response, snapshot, conversion |
| Integration and corrupt-data paths | No direct equivalent | 43 `#[test]` / `#[tokio::test]` | Fake app dispatch, HTTP dispatch, corrupt data |
| **Total** | **No direct equivalent** | **205 Rust tests** | Scoped API has broad unit and integration coverage |

### Test Gaps

- Handler-level tests for several query methods still rely on fake-app
  integration tests rather than direct unit tests.
- End-to-end HTTP coverage is intentionally smaller than pure transformation
  coverage because most handlers require populated app, DB, and bucket state.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 30 |
| Gaps (None + Partial) | 0 |
| Intentional Omissions | 5 |
| **Parity** | **30 / (30 + 0) = 100%** |
