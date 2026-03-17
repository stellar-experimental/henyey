# stellar-rpc Parity Status

**Crate**: `henyey-rpc`
**Upstream**: `stellar/stellar-rpc` (Go, GitHub)
**Overall Parity**: 100%
**Last Updated**: 2026-03-17

## Summary

| Area | Status | Notes |
|------|--------|-------|
| JSON-RPC 2.0 envelope | Full | Request/response parsing, error codes, body size limit, batch rejection |
| Method dispatch | Full | All 12 methods registered |
| getHealth | Full | Latency check via configurable `max_healthy_ledger_latency_secs` |
| getNetwork | Full | Passphrase, friendbot URL, protocol version |
| getLatestLedger | Full | All fields: `id`, `sequence`, `protocolVersion`, `closeTime`, `headerXdr`, `metadataXdr` |
| getVersionInfo | Full | `commitHash`, `buildTimestamp` from build.rs; `captiveCoreVersion` = henyey version |
| getFeeStats | Full | Sliding window, nearest-rank percentiles, classic + soroban fees |
| getLedgerEntries | Full | Core lookup + TTL; `xdrFormat` support; max 200 keys limit |
| getTransaction | Full | Full lookup, `xdrFormat` support, all fields |
| getTransactions | Full | Range query, TOID cursor, `xdrFormat` support, DB-level status filter |
| getLedgers | Full | Range query, cursor pagination, `xdrFormat` support |
| getEvents | Full | Core query; `xdrFormat` support; filter limits; `**` wildcard; diagnostic type rejected |
| sendTransaction | Full | Submission; `xdrFormat` support; actual error codes from herder |
| simulateTransaction | Full | InvokeHostFunction + ExtendTTL + Restore; `xdrFormat`; authMode; resourceConfig; stateChanges; memo validation |
| Infrastructure | Full | `xdrFormat` JSON output; 512KB body size limit; batch request rejection |

## File Mapping

| stellar-rpc File | Rust Module | Notes |
|-------------------|-------------|-------|
| `cmd/soroban-rpc/internal/methods/health.go` | `methods/health.rs` | |
| `cmd/soroban-rpc/internal/methods/get_network.go` | `methods/network.rs` | |
| `cmd/soroban-rpc/internal/methods/get_latest_ledger.go` | `methods/latest_ledger.rs` | |
| `cmd/soroban-rpc/internal/methods/get_version_info.go` | `methods/version_info.rs` | |
| `cmd/soroban-rpc/internal/methods/get_fee_stats.go` | `methods/fee_stats.rs` | Full sliding window |
| `cmd/soroban-rpc/internal/methods/get_ledger_entries.go` | `methods/get_ledger_entries.rs` | |
| `cmd/soroban-rpc/internal/methods/get_transaction.go` | `methods/get_transaction.rs` | |
| `cmd/soroban-rpc/internal/methods/get_transactions.go` | `methods/get_transactions.rs` | |
| `cmd/soroban-rpc/internal/methods/get_ledgers.go` | `methods/get_ledgers.rs` | |
| `cmd/soroban-rpc/internal/methods/get_events.go` | `methods/get_events.rs` | |
| `cmd/soroban-rpc/internal/methods/send_transaction.go` | `methods/send_transaction.rs` | |
| `cmd/soroban-rpc/internal/methods/simulate_transaction.go` | `methods/simulate_transaction.rs` | Delegates to `simulate/` |
| `cmd/soroban-rpc/internal/preflight/` | `simulate/mod.rs`, `simulate/snapshot.rs` | Soroban host invocation |
| `cmd/soroban-rpc/internal/feewindow/` | `fee_window.rs` | Ring buffer, percentile distribution |
| `cmd/soroban-rpc/internal/ingest/` | — | Handled by henyey-app/henyey-db |

## Component Mapping

### JSON-RPC Envelope (`server.rs`, `types/jsonrpc.rs`, `error.rs`, `dispatch.rs`)

Corresponds to: `cmd/soroban-rpc/internal/jsonrpc.go`, `cmd/soroban-rpc/internal/methods/`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| JSON-RPC 2.0 request parsing | `JsonRpcRequest` | Full |
| JSON-RPC 2.0 response envelope | `JsonRpcResponse` | Full |
| Error codes (-32600, -32601, -32602, -32603) | `JsonRpcError` | Full |
| Method dispatch by name | `dispatch()` | Full |
| HTTP body size limit (512KB) | `DefaultBodyLimit::max()` in `server.rs` | Full |
| Batch request rejection (`[` prefix check) | `server.rs` handler | Full |
| xdrFormat parameter support | `util::parse_format()`, `XdrFormat` enum | Full |

### getHealth (`methods/health.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/health.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Return "healthy" status | `handle()` | Full |
| `latestLedger` field | `handle()` | Full |
| `oldestLedger` from DB retention | `util::oldest_ledger()` | Full |
| `ledgerRetentionWindow` | `DEFAULT_LEDGER_RETENTION_WINDOW` | Full |
| Ledger age check (`maxHealthyLedgerLatency`) | `handle()` via `SystemTime` + `max_healthy_ledger_latency_secs` | Full |

### getNetwork (`methods/network.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_network.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| `passphrase` | `handle()` | Full |
| `friendbotUrl` | `handle()` | Full |
| `protocolVersion` | `handle()` | Full |

### getLatestLedger (`methods/latest_ledger.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_latest_ledger.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| `id` (ledger hash) | `handle()` | Full |
| `sequence` | `handle()` | Full |
| `protocolVersion` | `handle()` | Full |
| `closeTime` | `handle()` | Full |
| `headerXdr` | `handle()` | Full |
| `metadataXdr` | `handle()` | Full |

### getVersionInfo (`methods/version_info.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_version_info.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| `version` | `handle()` | Full |
| `protocolVersion` | `handle()` | Full |
| `commitHash` | `handle()` via `AppInfo.commit_hash` (from `build.rs`) | Full |
| `buildTimestamp` | `handle()` via `AppInfo.build_timestamp` (from `build.rs`) | Full |
| `captiveCoreVersion` | `handle()` as `"henyey-v{version}"` | Full |

### getFeeStats (`methods/fee_stats.rs`, `fee_window.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_fee_stats.go`, `cmd/soroban-rpc/internal/feewindow/`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| `FeeWindow` sliding window tracking | `FeeWindow`, `LedgerBucketWindow` | Full |
| Nearest-rank percentile computation | `compute_fee_distribution()` | Full |
| `sorobanInclusionFee` distribution | `FeeWindows::soroban_stats()` | Full |
| `inclusionFee` distribution | `FeeWindows::classic_stats()` | Full |
| Background ingestion from DB | `fee_window_poller` task | Full |
| `latestLedger` | `handle()` | Full |

### getLedgerEntries (`methods/get_ledger_entries.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_ledger_entries.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Base64 XDR key decoding | `handle()` | Full |
| Bucket list snapshot lookup | `handle()` | Full |
| TTL lookup for contract data/code | `ttl_key_for_entry()` | Full |
| `liveUntilLedgerSeq` in response | `handle()` | Full |
| `lastModifiedLedgerSeq` | `handle()` | Full |
| `extXdr` field | `handle()` | Full |
| Max 200 keys limit | `handle()` | Full |
| `xdrFormat` JSON output | `handle()` | Full |

### getTransaction (`methods/get_transaction.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_transaction.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Hash-based lookup from DB | `handle()` | Full |
| `envelopeXdr` | `handle()` | Full |
| `resultXdr` (extracted from pair) | `extract_result_xdr()` | Full |
| `resultMetaXdr` | `handle()` | Full |
| Status determination (SUCCESS/FAILED) | `determine_tx_status()` | Full |
| NOT_FOUND response | `handle()` | Full |
| `ledger`, `applicationOrder`, `createdAt` | `handle()` | Full |
| `oldestLedger` from DB | `util::oldest_ledger()` | Full |
| `feeBump` field | `handle()` | Full |
| `diagnosticEventsXdr` | `util::extract_diagnostic_events()` | Full |
| `xdrFormat` JSON output | `handle()` | Full |

### getTransactions (`methods/get_transactions.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_transactions.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Range query by startLedger | `handle()` | Full |
| TOID-based cursor pagination | `util::validate_pagination()` | Full |
| Transaction envelope/result/meta per entry | `handle()` | Full |
| `feeBump` detection | `is_fee_bump_envelope()` | Full |
| `diagnosticEventsXdr` | `util::extract_diagnostic_events()` | Full |
| `txHash`, `ledger`, `createdAt`, `applicationOrder` | `handle()` | Full |
| `latestLedger`, `oldestLedger`, close times | `handle()` | Full |
| Filter by status (success/failed) | `handle()` + DB-level `status` column | Full |
| `xdrFormat` support | `handle()` | Full |

### getLedgers (`methods/get_ledgers.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_ledgers.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Range query by startLedger | `handle()` | Full |
| Cursor-based pagination (ledger seq) | `validate_ledger_pagination()` | Full |
| Ledger hash, sequence, close time | `handle()` | Full |
| `headerXdr` (LedgerHeaderHistoryEntry) | `handle()` | Full |
| `metadataXdr` (full LedgerCloseMeta) | `handle()` | Full |
| `latestLedger`, `oldestLedger`, close times | `handle()` | Full |
| `xdrFormat` support | `handle()` | Full |

### getEvents (`methods/get_events.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/get_events.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| `startLedger` parameter | `handle()` | Full |
| `endLedger` parameter | `handle()` | Full |
| Event type filter (contract/system) | `parse_event_filters()` | Full |
| `contractIds` filter | `parse_event_filters()` | Full |
| Topic filters with OR alternatives | `parse_event_filters()` | Full |
| Pagination with cursor and limit | `handle()` | Full |
| DB query for events | `handle()` | Full |
| Ledger close time lookup | `get_ledger_close_time()` | Full |
| Event value XDR extraction | `extract_event_value()` | Full |
| Max 5 filters enforcement | `parse_event_filters()` | Full |
| Max 5 contractIDs per filter | `parse_event_filters()` | Full |
| Max 5 topics, 4 segments per topic | `parse_event_filters()` | Full |
| `**` wildcard support (topic truncation) | `parse_event_filters()` + DB-side break | Full |
| `diagnostic` type rejected | `parse_event_filters()` | Full |
| `oldestLedger` from DB | `util::oldest_ledger()` | Full |
| `xdrFormat` JSON output | `handle()` | Full |

### sendTransaction (`methods/send_transaction.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/send_transaction.go`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| Base64 envelope decoding | `handle()` | Full |
| Transaction hash computation | `handle()` | Full |
| Herder submission | `handle()` | Full |
| PENDING/DUPLICATE/TRY_AGAIN_LATER/ERROR status | `handle()` | Full |
| `errorResultXdr` with actual error code | `handle()` via `TxResultCode` from herder | Full |
| `diagnosticEventsXdr` | Empty array for all ERROR outcomes | Full |
| `xdrFormat` JSON output | `handle()` | Full |

### simulateTransaction (`simulate/mod.rs`, `simulate/snapshot.rs`)

Corresponds to: `cmd/soroban-rpc/internal/methods/simulate_transaction.go`, `cmd/soroban-rpc/internal/preflight/`

| stellar-rpc | Rust | Status |
|-------------|------|--------|
| InvokeHostFunction simulation | `handle_invoke()` | Full |
| Recording mode invocation | `run_invoke_simulation()` | Full |
| BucketList snapshot source | `BucketListSnapshotSource` | Full |
| TTL-aware entry lookup | `get_entry_ttl()` | Full |
| Resource adjustment (1.04x instructions, 1.0x read/write bytes) | `adjust_resources()` | Full |
| Refundable fee adjustment (1.15x) | `compute_invoke_resource_fee()` | Full |
| SorobanTransactionData construction | `build_invoke_response()` | Full |
| Auth entries in response | `build_invoke_response()` | Full |
| Return value XDR | `build_invoke_response()` | Full |
| Error response with cost | `build_error_response()` | Full |
| ExtendFootprintTtl simulation | `simulate_extend_ttl_op()` | Full |
| RestoreFootprint simulation | `simulate_restore_op()` | Full |
| Rent fee computation | `compute_resource_fee_with_rent()` | Full |
| `xdrFormat` support | `handle()` | Full |
| `authMode` parameter (enforce/record/record_allow_nonroot) | `resolve_auth_mode()` | Full |
| `stateChanges` in response (created/updated/deleted diffs) | `extract_modified_entries()`, `serialize_state_changes()` | Full |
| `resourceConfig.instructionLeeway` parameter | `handle()` + `adjust_resources()` | Full |
| Memo validation (MemoText ≤ 28 bytes) | `validate_memo()` | Full |

## Intentional Omissions

| stellar-rpc Component | Reason |
|------------------------|--------|
| Ingestion pipeline (`internal/ingest/`) | Handled by `henyey-app` and `henyey-db`; not part of the RPC crate |
| Database layer (`internal/db/`) | Handled by `henyey-db` |
| Captive core management | henyey is a full node, not a captive-core wrapper |
| Prometheus metrics endpoint | Out of scope for initial implementation |
| CORS / HTTP middleware | Not required for node-internal RPC |

## Architectural Differences

1. **Language and framework**
   - **stellar-rpc**: Go, `jrpc2` library, HTTP handler middleware
   - **Rust**: Axum web framework, manual JSON-RPC dispatch
   - **Rationale**: Axum is idiomatic Rust; manual dispatch is simpler for 12 methods

2. **Simulation execution**
   - **stellar-rpc**: Calls into `soroban-simulation` (C/Go bridge via CGo)
   - **Rust**: Directly invokes `soroban-env-host-p25` in `spawn_blocking`
   - **Rationale**: No CGo bridge needed; Rust can call soroban-env-host natively

3. **State access**
   - **stellar-rpc**: Maintains its own ledger entry reader backed by a DB
   - **Rust**: Reads directly from `SearchableBucketListSnapshot` (in-memory)
   - **Rationale**: Faster reads; bucket list snapshots are already maintained by henyey-bucket

4. **Integrated vs standalone**
   - **stellar-rpc**: Standalone service that connects to a captive stellar-core instance
   - **Rust**: Embedded module within the henyey node process
   - **Rationale**: Simpler deployment; direct access to `App` state without IPC

5. **Fee window ingestion**
   - **stellar-rpc**: Hooks into ingestion pipeline via `InsertFn` callback
   - **Rust**: Background poller reads new LCMs from DB every second
   - **Rationale**: Avoids cross-crate coupling; DB is the source of truth

## Test Coverage

| Area | stellar-rpc Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| JSON-RPC envelope | Extensive integration tests | 10 `#[test]` in `server.rs` | Parsing, serialization, batch detection, version validation |
| TOID encoding/pagination/util | N/A (inline in Go) | 22 `#[test]` in `util.rs` | TOID, pagination, tx status, timestamps, TTL keys, xdrFormat |
| Fee window | Unit + integration tests | 13 `#[test]` in `fee_window.rs` | Distribution, ring buffer, fee extraction, ops counting |
| simulateTransaction | Integration + preflight tests | 44 `#[test]` in `simulate/mod.rs` | sim_adjust, resources, auth, extract_op, XDR fields, state changes, fees, tx size |
| Snapshot normalization | Unit tests | 8 `#[test]` in `simulate/snapshot.rs` | V0/V1/V2→V3 upgrade, signers, XDR size |
| getEvents | Integration tests with filters | 8 `#[test]` in `methods/get_events.rs` | Filter parsing, limits, wildcards, type rejection |
| sendTransaction | Integration tests | 3 `#[test]` in `methods/send_transaction.rs` | Error result construction, diagnostic events, error codes |
| getHealth | Integration test | 0 | Handler requires `RpcContext` |
| getLedgerEntries | Integration tests with TTL | 0 | Handler requires `RpcContext` |

**Total: 108 unit tests.**

### Test Gaps

- No handler-level integration tests (handlers require `RpcContext` with a running `App`)
- No integration tests for the full RPC server request/response cycle
- stellar-rpc has extensive integration tests that exercise the full request/response cycle

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 107 |
| Gaps (None + Partial) | 0 |
| Intentional Omissions | 5 |
| **Parity** | **107 / 107 = 100%** |
